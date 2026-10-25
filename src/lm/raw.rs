//! The raw completions route: the prompt is rendered here in Qwen 3.5's chat
//! format, which is what lets us start the assistant turn with an empty
//! `<think></think>` block and so switch its thinking off. The model's reply is
//! plain text, so its `<tool_call>` blocks are parsed here too.
//!
//! The format follows the model's own `chat_template.jinja`:
//!
//! ```text
//! <|im_start|>system
//! # Tools ... <tools>{json}</tools> ... instructions
//!
//! <system prompt><|im_end|>
//! <|im_start|>user
//! ...<|im_end|>
//! <|im_start|>assistant
//! <think>
//! reasoning
//! </think>
//!
//! text<tool_call>
//! <function=name>
//! <parameter=key>
//! value
//! </parameter>
//! </function>
//! </tool_call><|im_end|>
//! <|im_start|>user
//! <tool_response>
//! result
//! </tool_response><|im_end|>
//! <|im_start|>assistant
//! <think>
//! ```

use serde_json::{Value, json};

use super::sse::each_payload;
use super::{
    Endpoint, LmError, Message, Reply, Request, Sender, StreamEvent, ToolCall, ToolSpec, send,
    usage_of,
};

const TOOLS_INSTRUCTIONS: &str = "\n\nIf you choose to call a function ONLY reply in the following format with NO suffix:\n\n<tool_call>\n<function=example_function_name>\n<parameter=example_parameter_1>\nvalue_1\n</parameter>\n<parameter=example_parameter_2>\nThis is the value for the second parameter\nthat can span\nmultiple lines\n</parameter>\n</function>\n</tool_call>\n\n<IMPORTANT>\nReminder:\n- Function calls MUST follow the specified format: an inner <function=...></function> block must be nested within <tool_call></tool_call> XML tags\n- Required parameters MUST be specified\n- You may provide optional reasoning for your function call in natural language BEFORE the function call, but NOT after\n- If there is no function call available, answer the question like normal with your current knowledge and do not tell the user about function calls\n</IMPORTANT>";

pub struct RawModel {
    pub endpoint: Endpoint,
    /// Think before answering? Off pre-fills an empty think block.
    pub thinking: bool,
    client: reqwest::Client,
}

impl RawModel {
    pub fn new(endpoint: Endpoint, thinking: bool) -> Self {
        Self {
            endpoint,
            thinking,
            client: reqwest::Client::new(),
        }
    }

    pub async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
        let prompt = render(&req.messages, &req.tools, self.thinking);
        let body = json!({
            "model": self.endpoint.model,
            "prompt": prompt,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": req.max_tokens,
            // The template's own end-of-turn marker; the model may also stop on its own.
            "stop": ["<|im_end|>"],
        });
        let res = send(&self.endpoint, &self.client, "/v1/completions", &body).await?;

        let mut reply = Reply::default();
        let mut parser = Parser::new(self.thinking, &req.tools);
        each_payload(res.bytes_stream(), |payload| {
            let v: Value =
                serde_json::from_str(payload).map_err(|e| LmError::Malformed(e.to_string()))?;
            if let Some(usage) = usage_of(&v) {
                reply.usage = usage;
            }
            if let Some(text) = v.pointer("/choices/0/text").and_then(Value::as_str) {
                for evt in parser.push(text) {
                    reply.take(&evt);
                    let _ = tx.try_send(evt);
                }
            }
            Ok(())
        })
        .await?;
        for evt in parser.finish() {
            reply.take(&evt);
            let _ = tx.try_send(evt);
        }
        // This route counts every generated token as completion; it knows nothing
        // of the think block. Split the count by share of characters, close enough
        // for a cost line.
        if reply.usage.reasoning_tokens == 0 && !reply.reasoning.is_empty() {
            let total = (reply.reasoning.len() + reply.content.len()).max(1) as f64;
            let share = reply.reasoning.len() as f64 / total;
            reply.usage.reasoning_tokens =
                (reply.usage.completion_tokens as f64 * share).round() as u32;
        }
        Ok(reply)
    }
}

// ---- Rendering --------------------------------------------------------------

/// The whole prompt, ending with the start of the assistant's turn.
pub fn render(messages: &[Message], tools: &[ToolSpec], thinking: bool) -> String {
    let mut out = String::new();
    let system = match messages.first() {
        Some(Message::System(s)) => Some(s.trim()),
        _ => None,
    };
    if !tools.is_empty() {
        out.push_str(
            "<|im_start|>system\n# Tools\n\nYou have access to the following functions:\n\n<tools>",
        );
        for tool in tools {
            out.push('\n');
            out.push_str(&serde_json::to_string(tool).unwrap_or_default());
        }
        out.push_str("\n</tools>");
        out.push_str(TOOLS_INSTRUCTIONS);
        if let Some(s) = system.filter(|s| !s.is_empty()) {
            out.push_str("\n\n");
            out.push_str(s);
        }
        out.push_str("<|im_end|>\n");
    } else if let Some(s) = system {
        out.push_str(&format!("<|im_start|>system\n{s}<|im_end|>\n"));
    }

    // Reasoning is kept only for assistant turns after the last real user
    // message (tool results do not count), as the template does.
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .unwrap_or(0);

    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::System(_) => {}
            Message::User(s) => {
                out.push_str(&format!("<|im_start|>user\n{}<|im_end|>\n", s.trim()))
            }
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                out.push_str("<|im_start|>assistant\n");
                let content = content.trim();
                if i > last_user {
                    let r = reasoning.as_deref().unwrap_or("").trim();
                    out.push_str(&format!("<think>\n{r}\n</think>\n\n"));
                }
                out.push_str(content);
                for (n, call) in tool_calls.iter().enumerate() {
                    if n == 0 && !content.is_empty() {
                        out.push_str("\n\n");
                    } else if n > 0 {
                        out.push('\n');
                    }
                    out.push_str(&render_call(call));
                }
                out.push_str("<|im_end|>\n");
            }
            Message::Tool { .. } => {
                // A run of tool results is one user turn.
                out.push_str("<|im_start|>user");
                while let Some(Message::Tool { content, .. }) = messages.get(i) {
                    out.push_str(&format!(
                        "\n<tool_response>\n{}\n</tool_response>",
                        content.trim()
                    ));
                    i += 1;
                }
                out.push_str("<|im_end|>\n");
                continue;
            }
        }
        i += 1;
    }

    out.push_str("<|im_start|>assistant\n");
    out.push_str(if thinking {
        "<think>\n"
    } else {
        "<think>\n\n</think>\n\n"
    });
    out
}

fn render_call(call: &ToolCall) -> String {
    let mut s = format!("<tool_call>\n<function={}>\n", call.name);
    if let Some(args) = call.arguments.as_object() {
        for (k, v) in args {
            let text = match v {
                Value::String(t) => t.clone(),
                other => other.to_string(),
            };
            s.push_str(&format!("<parameter={k}>\n{text}\n</parameter>\n"));
        }
    }
    s.push_str("</function>\n</tool_call>");
    s
}

// ---- Parsing ----------------------------------------------------------------

/// Turns the model's streamed text into events: reasoning until `</think>`,
/// text outside tool calls, and one `ToolCall` per complete block. Text is held
/// back only as far as needed to be sure a tag is not about to start.
pub struct Parser<'a> {
    in_reasoning: bool,
    /// Drop newlines at the start of the text: the format puts some after the
    /// think block and between calls, and the model often adds more.
    skip_newlines: bool,
    buf: String,
    tools: &'a [ToolSpec],
    next_id: usize,
}

const THINK_END: &str = "</think>";
const CALL_START: &str = "<tool_call>";
const CALL_END: &str = "</tool_call>";

impl<'a> Parser<'a> {
    // Rust: the `'a` lifetime says the parser borrows the tool list and cannot
    // outlive it. The compiler checks that for us.
    pub fn new(thinking: bool, tools: &'a [ToolSpec]) -> Self {
        Self {
            in_reasoning: thinking,
            skip_newlines: true,
            buf: String::new(),
            tools,
            next_id: 1,
        }
    }

    pub fn push(&mut self, text: &str) -> Vec<StreamEvent> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        loop {
            if self.in_reasoning {
                if let Some(at) = self.buf.find(THINK_END) {
                    let reasoning: String = self.buf.drain(..at).collect();
                    self.buf.drain(..THINK_END.len());
                    self.in_reasoning = false;
                    self.skip_newlines = true;
                    push_text(&mut out, StreamEvent::Reasoning, reasoning);
                    continue;
                }
                let safe = self.buf.len() - tail_held_back(&self.buf, THINK_END);
                let reasoning: String = self.buf.drain(..safe).collect();
                push_text(&mut out, StreamEvent::Reasoning, reasoning);
                return out;
            }
            if self.skip_newlines {
                let lead = self.buf.len() - self.buf.trim_start_matches('\n').len();
                self.buf.drain(..lead);
                if self.buf.is_empty() {
                    return out; // more newlines may follow; keep skipping
                }
                self.skip_newlines = false;
            }
            if let Some(start) = self.buf.find(CALL_START) {
                let Some(end) = self.buf[start..].find(CALL_END) else {
                    // The block is still arriving: emit the text before it and wait.
                    let text: String = self.buf.drain(..start).collect();
                    push_text(&mut out, StreamEvent::Text, text);
                    return out;
                };
                let text: String = self.buf.drain(..start).collect();
                push_text(&mut out, StreamEvent::Text, text);
                let block: String = self.buf.drain(..end + CALL_END.len()).collect();
                let inner = &block[CALL_START.len()..block.len() - CALL_END.len()];
                if let Some(call) = parse_call(inner, self.tools, self.next_id) {
                    self.next_id += 1;
                    out.push(StreamEvent::ToolCall(call));
                }
                // A newline between calls belongs to the format, not the text.
                self.skip_newlines = true;
                continue;
            }
            let safe = self.buf.len() - tail_held_back(&self.buf, CALL_START);
            let text: String = self.buf.drain(..safe).collect();
            push_text(&mut out, StreamEvent::Text, text);
            return out;
        }
    }

    /// The stream has ended: whatever is held back is text (or reasoning).
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        let rest = std::mem::take(&mut self.buf);
        let mut out = Vec::new();
        if self.in_reasoning {
            push_text(&mut out, StreamEvent::Reasoning, rest);
        } else {
            push_text(&mut out, StreamEvent::Text, rest.trim_end().to_string());
        }
        out
    }
}

fn push_text(out: &mut Vec<StreamEvent>, make: fn(String) -> StreamEvent, text: String) {
    if !text.is_empty() {
        out.push(make(text));
    }
}

/// How many bytes at the end of `buf` could be the start of `tag`, and so must wait.
fn tail_held_back(buf: &str, tag: &str) -> usize {
    let max = tag.len().min(buf.len());
    (1..=max)
        .rev()
        .find(|&n| buf.is_char_boundary(buf.len() - n) && tag.starts_with(&buf[buf.len() - n..]))
        .unwrap_or(0)
}

/// `<function=name>\n<parameter=k>\nv\n</parameter>\n</function>` → a call, with
/// each value coerced to the type the tool's schema declares.
fn parse_call(inner: &str, tools: &[ToolSpec], id: usize) -> Option<ToolCall> {
    let inner = inner.trim();
    let name_start = inner.find("<function=")? + "<function=".len();
    let name_end = inner[name_start..].find('>')? + name_start;
    let name = inner[name_start..name_end].trim().to_string();
    let body = &inner[name_end + 1..];
    let body = body.strip_suffix("</function>").unwrap_or(body);

    let schema = tools.iter().find(|t| t.name == name).map(|t| &t.parameters);
    let mut args = serde_json::Map::new();
    let mut rest = body;
    while let Some(p) = rest.find("<parameter=") {
        let key_start = p + "<parameter=".len();
        let Some(key_len) = rest[key_start..].find('>') else {
            break;
        };
        let key = rest[key_start..key_start + key_len].trim().to_string();
        let value_start = key_start + key_len + 1;
        let Some(value_len) = rest[value_start..].find("</parameter>") else {
            break;
        };
        let raw = &rest[value_start..value_start + value_len];
        // One newline each side belongs to the format; inner newlines are the value's.
        let raw = raw.strip_prefix('\n').unwrap_or(raw);
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        args.insert(
            key.clone(),
            coerce(
                raw,
                schema.and_then(|s| s.pointer(&format!("/properties/{key}/type"))),
            ),
        );
        rest = &rest[value_start + value_len + "</parameter>".len()..];
    }
    Some(ToolCall {
        id: format!("call_{id}"),
        name,
        arguments: Value::Object(args),
    })
}

/// A parameter's text as the JSON type its schema asks for. Strings stay strings
/// even when they look like numbers.
fn coerce(raw: &str, ty: Option<&Value>) -> Value {
    let ty = ty.and_then(Value::as_str).unwrap_or("string");
    let t = raw.trim();
    match ty {
        "integer" => t
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        "number" => t
            .parse::<f64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(raw.to_string())),
        "boolean" => match t {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw.to_string()),
        },
        "object" | "array" => {
            serde_json::from_str(t).unwrap_or_else(|_| Value::String(raw.to_string()))
        }
        _ => Value::String(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": {"type": "string"}, "start": {"type": "integer"}, "all": {"type": "boolean"} },
                "required": ["path"]
            }),
        }]
    }

    #[test]
    fn renders_a_plain_turn_with_thinking_off() {
        let prompt = render(
            &[
                Message::System("Be brief.".into()),
                Message::User("Hi".into()),
            ],
            &[],
            false,
        );
        assert_eq!(
            prompt,
            "<|im_start|>system\nBe brief.<|im_end|>\n<|im_start|>user\nHi<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
    }

    #[test]
    fn renders_tools_into_the_system_turn_and_keeps_recent_reasoning() {
        let messages = vec![
            Message::System("Rules.".into()),
            Message::User("Read a".into()),
            Message::Assistant {
                content: "".into(),
                reasoning: Some("I should read it.".into()),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a", "start": 3}),
                }],
            },
            Message::Tool {
                call_id: "call_1".into(),
                content: "line 3".into(),
            },
        ];
        let prompt = render(&messages, &tools(), true);
        assert!(prompt.starts_with("<|im_start|>system\n# Tools\n\nYou have access to the following functions:\n\n<tools>\n{\"name\":\"read_file\""));
        assert!(
            prompt
                .contains("</IMPORTANT>\n\nRules.<|im_end|>\n<|im_start|>user\nRead a<|im_end|>\n")
        );
        assert!(prompt.contains(
            "<|im_start|>assistant\n<think>\nI should read it.\n</think>\n\n<tool_call>\n<function=read_file>\n<parameter=path>\na\n</parameter>\n<parameter=start>\n3\n</parameter>\n</function>\n</tool_call><|im_end|>\n"
        ));
        assert!(prompt.ends_with("<|im_start|>user\n<tool_response>\nline 3\n</tool_response><|im_end|>\n<|im_start|>assistant\n<think>\n"));
    }

    #[test]
    fn drops_reasoning_from_turns_before_the_last_user_message() {
        let messages = vec![
            Message::User("one".into()),
            Message::Assistant {
                content: "A".into(),
                reasoning: Some("old".into()),
                tool_calls: vec![],
            },
            Message::User("two".into()),
        ];
        let prompt = render(&messages, &[], false);
        assert!(prompt.contains("<|im_start|>assistant\nA<|im_end|>\n"));
        assert!(!prompt.contains("old"));
    }

    #[test]
    fn parses_reasoning_then_text_then_a_call_across_chunks() {
        let tools = tools();
        let mut p = Parser::new(true, &tools);
        let mut events = Vec::new();
        for piece in [
            "Let me ",
            "look.</thi",
            "nk>\n\nI'll read it.",
            "<tool_call>\n<function=read_file>\n<parameter=path>\nnotes.txt\n</parameter>\n<parameter=start>\n12\n</parameter>\n</function>\n</tool_",
            "call>",
        ] {
            events.extend(p.push(piece));
        }
        events.extend(p.finish());
        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("Let me ".into()),
                StreamEvent::Reasoning("look.".into()),
                StreamEvent::Text("I'll read it.".into()),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "notes.txt", "start": 12})
                }),
            ]
        );
    }

    #[test]
    fn holds_back_a_possible_tag_start_and_releases_it() {
        let tools = tools();
        let mut p = Parser::new(false, &tools);
        assert_eq!(p.push("see <to"), vec![StreamEvent::Text("see ".into())]);
        assert_eq!(p.push("ol b"), vec![StreamEvent::Text("<tool b".into())]);
        assert_eq!(
            p.push("a < b"),
            vec![StreamEvent::Text("a < b".into())],
            "'< ' cannot start a tag"
        );
        assert!(p.finish().is_empty());
    }

    #[test]
    fn two_calls_in_one_turn_get_distinct_ids() {
        let tools = tools();
        let mut p = Parser::new(false, &tools);
        let text = "<tool_call>\n<function=read_file>\n<parameter=path>\na\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=read_file>\n<parameter=path>\nb\n</parameter>\n<parameter=all>\ntrue\n</parameter>\n</function>\n</tool_call>";
        let events = p.push(text);
        let ids: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolCall(c) => Some(c.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["call_1", "call_2"]);
        assert_eq!(
            events.len(),
            2,
            "no stray text between the calls: {events:?}"
        );
        if let StreamEvent::ToolCall(c) = &events[1] {
            assert_eq!(c.arguments["all"], true);
        }
    }

    #[test]
    fn drops_newlines_after_the_think_block_even_across_chunks() {
        let mut p = Parser::new(true, &[]);
        let mut events = Vec::new();
        for piece in ["ok</think>\n\n", "\n", "\nEarth", "\nis round"] {
            events.extend(p.push(piece));
        }
        events.extend(p.finish());
        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("ok".into()),
                StreamEvent::Text("Earth".into()),
                StreamEvent::Text("\nis round".into()),
            ]
        );
    }

    #[test]
    fn coerces_by_schema() {
        assert_eq!(coerce("12", Some(&json!("integer"))), json!(12));
        assert_eq!(coerce("12", None), json!("12"));
        assert_eq!(coerce("x", Some(&json!("integer"))), json!("x"));
        assert_eq!(coerce("[1,2]", Some(&json!("array"))), json!([1, 2]));
    }
}
