//! The OpenAI-style chat route: LM Studio renders the prompt and parses tool calls.

use serde_json::{Value, json};

use super::sse::each_payload;
use super::{
    Effort, Endpoint, LmError, Message, Reply, Request, Sender, StreamEvent, ToolCall, send,
    usage_of,
};

pub struct ChatModel {
    pub endpoint: Endpoint,
    pub effort: Option<Effort>,
    client: reqwest::Client,
}

impl ChatModel {
    pub fn new(endpoint: Endpoint, effort: Option<Effort>) -> Self {
        Self {
            endpoint,
            effort,
            client: reqwest::Client::new(),
        }
    }

    /// Stream one turn. Events go to `tx` as they arrive; the whole reply comes back at the end.
    pub async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
        let body = self.body(req);
        let res = send(&self.endpoint, &self.client, "/v1/chat/completions", &body).await?;

        let mut reply = Reply::default();
        let mut calls = Calls::default();
        // Rust: the closure borrows `reply` and `calls` mutably for as long as
        // `each_payload` runs; nothing else may touch them until it returns.
        each_payload(res.bytes_stream(), |payload| {
            let v: Value =
                serde_json::from_str(payload).map_err(|e| LmError::Malformed(e.to_string()))?;
            if let Some(usage) = usage_of(&v) {
                reply.usage = usage;
            }
            let Some(choice) = v.pointer("/choices/0") else {
                return Ok(());
            };
            if let Some(delta) = choice.get("delta") {
                for evt in delta_events(delta, &mut calls) {
                    reply.take(&evt);
                    // Rust: `try_send` never waits. A full channel means the UI
                    // fell behind; dropping a piece of text is better than
                    // stalling the model, and the reply keeps the full text anyway.
                    let _ = tx.try_send(evt);
                }
            }
            if choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .is_some()
            {
                for call in calls.finish() {
                    reply.take(&StreamEvent::ToolCall(call.clone()));
                    let _ = tx.try_send(StreamEvent::ToolCall(call));
                }
            }
            Ok(())
        })
        .await?;
        Ok(reply)
    }

    fn body(&self, req: &Request) -> Value {
        let mut body = json!({
            "model": self.endpoint.model,
            "messages": req.messages.iter().map(message_json).collect::<Vec<_>>(),
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": req.max_tokens,
        });
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({ "type": "function", "function": t }))
                .collect();
        }
        if let Some(effort) = self.effort {
            body["reasoning_effort"] = json!(effort.as_str());
        }
        body
    }
}

/// A message in the chat route's JSON.
fn message_json(m: &Message) -> Value {
    match m {
        Message::System(s) => json!({ "role": "system", "content": s }),
        Message::User(s) => json!({ "role": "user", "content": s }),
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
        } => {
            let mut v = json!({ "role": "assistant", "content": content });
            if let Some(r) = reasoning {
                v["reasoning_content"] = json!(r);
            }
            if !tool_calls.is_empty() {
                v["tool_calls"] = tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments.to_string() }
                        })
                    })
                    .collect();
            }
            v
        }
        Message::Tool { call_id, content } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": content })
        }
    }
}

/// Tool calls arrive in pieces: an `id` and `name` first, then the arguments as
/// text fragments, keyed by `index`. This gathers them until the turn ends.
#[derive(Default)]
struct Calls {
    partial: Vec<(String, String, String)>, // (id, name, arguments so far), by index
}

impl Calls {
    fn add(&mut self, piece: &Value) {
        let index = piece.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while self.partial.len() <= index {
            self.partial.push(Default::default());
        }
        let slot = &mut self.partial[index];
        if let Some(id) = piece.get("id").and_then(Value::as_str) {
            slot.0 = id.to_string();
        }
        if let Some(name) = piece.pointer("/function/name").and_then(Value::as_str) {
            slot.1.push_str(name);
        }
        if let Some(args) = piece.pointer("/function/arguments").and_then(Value::as_str) {
            slot.2.push_str(args);
        }
    }

    /// The completed calls, in order. Arguments that are not JSON become `{}`.
    fn finish(&mut self) -> Vec<ToolCall> {
        // Rust: `std::mem::take` swaps the Vec out for an empty one and gives us
        // the old contents, so `self` is left usable.
        std::mem::take(&mut self.partial)
            .into_iter()
            .enumerate()
            .filter(|(_, (_, name, _))| !name.is_empty())
            .map(|(i, (id, name, args))| ToolCall {
                id: if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                },
                name,
                arguments: serde_json::from_str(&args).unwrap_or_else(|_| json!({})),
            })
            .collect()
    }
}

/// The events in one streamed `delta`. Tool-call pieces are gathered, not emitted.
fn delta_events(delta: &Value, calls: &mut Calls) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str)
        && !r.is_empty()
    {
        out.push(StreamEvent::Reasoning(r.to_string()));
    }
    if let Some(c) = delta.get("content").and_then(Value::as_str)
        && !c.is_empty()
    {
        out.push(StreamEvent::Text(c.to_string()));
    }
    if let Some(pieces) = delta.get("tool_calls").and_then(Value::as_array) {
        for piece in pieces {
            calls.add(piece);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gathers_a_tool_call_from_pieces() {
        let mut calls = Calls::default();
        let d1 = json!({"tool_calls":[{"index":0,"id":"716522060","type":"function","function":{"name":"read_file","arguments":""}}]});
        let d2 = json!({"tool_calls":[{"index":0,"type":"function","function":{"arguments":"{\"path\":\"notes.txt\"}"}}]});
        assert!(delta_events(&d1, &mut calls).is_empty());
        assert!(delta_events(&d2, &mut calls).is_empty());
        assert_eq!(
            calls.finish(),
            vec![ToolCall {
                id: "716522060".into(),
                name: "read_file".into(),
                arguments: json!({"path": "notes.txt"})
            }]
        );
        assert!(calls.finish().is_empty(), "finish empties the gatherer");
    }

    #[test]
    fn splits_reasoning_from_text() {
        let mut calls = Calls::default();
        let d = json!({"role":"assistant","reasoning_content":"The"});
        assert_eq!(
            delta_events(&d, &mut calls),
            vec![StreamEvent::Reasoning("The".into())]
        );
        let d = json!({"content":"\n\n"});
        assert_eq!(
            delta_events(&d, &mut calls),
            vec![StreamEvent::Text("\n\n".into())]
        );
    }

    #[test]
    fn body_carries_tools_and_effort() {
        let model = ChatModel::new(
            Endpoint {
                url: "http://x".into(),
                api_token: None,
                model: "m".into(),
            },
            Some(Effort::Low),
        );
        let req = Request {
            messages: vec![Message::System("s".into()), Message::User("u".into())],
            tools: vec![crate::lm::ToolSpec {
                name: "t".into(),
                description: "d".into(),
                parameters: json!({"type":"object"}),
            }],
            max_tokens: 10,
        };
        let body = model.body(&req);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["tools"][0]["function"]["name"], "t");
        assert_eq!(body["messages"][1]["content"], "u");
    }

    #[test]
    fn assistant_message_carries_its_calls() {
        let m = Message::Assistant {
            content: "".into(),
            reasoning: None,
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "f".into(),
                arguments: json!({"a": 1}),
            }],
        };
        let v = message_json(&m);
        assert_eq!(v["tool_calls"][0]["function"]["arguments"], "{\"a\":1}");
        assert!(v.get("reasoning_content").is_none());
    }
}
