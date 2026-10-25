//! Talking to LM Studio. Two routes, one shape of result.
//!
//! * [`chat::ChatModel`]: the OpenAI-style `/v1/chat/completions` route. LM Studio
//!   renders the prompt, applies the model's reasoning-effort setting and hands
//!   back structured tool calls. Used for the planner.
//! * [`raw::RawModel`]: the `/v1/completions` route with the prompt rendered here,
//!   Qwen's own format. The only way to switch a model's thinking off when LM
//!   Studio does not expose the switch. Used for the worker.
//!
//! Both stream [`StreamEvent`]s into a channel, so the terminal can draw the
//! reasoning and text as they arrive and act on each tool call.

pub mod chat;
pub mod raw;
pub mod sse;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

/// One message in a conversation, in the shape the chat route expects.
// Rust: an enum where each variant carries its own fields. A `Message` is exactly
// one of these; `match` makes you handle every one. The serde attributes are
// for the transcript file: `{"role":"user","body":"…"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", content = "body", rename_all = "lowercase")]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// What a tool returned, for the call with this id.
    Tool {
        call_id: String,
        content: String,
    },
}

/// A tool offered to the model: a name, what it does, and a JSON Schema for its arguments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A call the model asked for. `arguments` is already parsed JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// How hard the planner thinks. LM Studio maps these onto the model's template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    Xhigh,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::Xhigh => "xhigh",
        }
    }
    // Rust: serde's `default = "path"` wants a function, so these give it one.
    pub fn low() -> Self {
        Effort::Low
    }
    pub fn medium() -> Self {
        Effort::Medium
    }
    pub fn xhigh() -> Self {
        Effort::Xhigh
    }
}

/// Token counts for one turn, as LM Studio reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub reasoning_tokens: u32,
}

/// What a model streams back, piece by piece.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A piece of the model's thinking.
    Reasoning(String),
    /// A piece of the answer.
    Text(String),
    /// A whole tool call, once its arguments are complete.
    ToolCall(ToolCall),
}

/// One request: the conversation so far and the tools on offer.
#[derive(Debug, Clone)]
pub struct Request {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

/// What a finished turn amounts to, built from the stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub reasoning: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

impl Reply {
    /// The assistant message to append to the conversation for the next round.
    pub fn message(&self) -> Message {
        Message::Assistant {
            content: self.content.clone(),
            reasoning: (!self.reasoning.is_empty()).then(|| self.reasoning.clone()),
            tool_calls: self.tool_calls.clone(),
        }
    }

    /// Fold one event into the running reply.
    pub fn take(&mut self, evt: &StreamEvent) {
        match evt {
            StreamEvent::Reasoning(s) => self.reasoning.push_str(s),
            StreamEvent::Text(s) => self.content.push_str(s),
            StreamEvent::ToolCall(c) => self.tool_calls.push(c.clone()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LmError {
    #[error("LM Studio is not reachable at {url}: {source}")]
    Unreachable { url: String, source: reqwest::Error },
    #[error("LM Studio answered {status}: {detail}")]
    Http { status: u16, detail: String },
    #[error("the stream broke off: {0}")]
    Stream(String),
    #[error("the model's reply could not be read: {0}")]
    Malformed(String),
}

pub type Sender = mpsc::Sender<StreamEvent>;

/// Settings shared by both routes.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub url: String,
    pub api_token: Option<String>,
    pub model: String,
}

impl Endpoint {
    fn request(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        let mut req = client
            .post(format!("{}{path}", self.url.trim_end_matches('/')))
            .header("Content-Type", "application/json");
        if let Some(token) = &self.api_token {
            req = req.bearer_auth(token.trim());
        }
        req
    }
}

/// The models LM Studio has loaded right now. `/api/v0/models` lists every
/// model it knows with a `state`; the OpenAI-style list does not say.
pub async fn loaded_models(endpoint: &Endpoint) -> Result<Vec<String>, LmError> {
    let client = reqwest::Client::new();
    let mut req = client.get(format!(
        "{}/api/v0/models",
        endpoint.url.trim_end_matches('/')
    ));
    if let Some(token) = &endpoint.api_token {
        req = req.bearer_auth(token.trim());
    }
    let res = req.send().await.map_err(|source| LmError::Unreachable {
        url: endpoint.url.clone(),
        source,
    })?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        return Err(LmError::Http {
            status,
            detail: http_detail(&text),
        });
    }
    let body: Value = res
        .json()
        .await
        .map_err(|e| LmError::Stream(e.to_string()))?;
    let loaded = body["data"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter(|m| m["state"].as_str() == Some("loaded"))
                .filter_map(|m| m["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(loaded)
}

/// Send a request and hand back the response, or a readable error.
async fn send(
    endpoint: &Endpoint,
    client: &reqwest::Client,
    path: &str,
    body: &Value,
) -> Result<reqwest::Response, LmError> {
    let res = endpoint
        .request(client, path)
        .json(body)
        .send()
        .await
        .map_err(|source| LmError::Unreachable {
            url: endpoint.url.clone(),
            source,
        })?;
    if !res.status().is_success() {
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        return Err(LmError::Http {
            status,
            detail: http_detail(&text),
        });
    }
    Ok(res)
}

/// LM Studio's error bodies are `{"error": {"message": ...}}` or `{"error": "..."}`.
fn http_detail(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        let e = v.get("error")?;
        e.get("message")
            .and_then(Value::as_str)
            .or_else(|| e.as_str())
            .map(str::to_string)
    });
    message.unwrap_or_else(|| body.chars().take(300).collect())
}

/// Read LM Studio's `usage` object, whichever route it came from.
fn usage_of(v: &Value) -> Option<Usage> {
    let u = v.get("usage")?;
    if u.is_null() {
        return None;
    }
    let n = |key: &str| u.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
    Some(Usage {
        prompt_tokens: n("prompt_tokens"),
        completion_tokens: n("completion_tokens"),
        reasoning_tokens: u
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_error_shapes() {
        assert_eq!(
            http_detail(r#"{"error":{"message":"no such model"}}"#),
            "no such model"
        );
        assert_eq!(http_detail(r#"{"error":"plain"}"#), "plain");
        assert_eq!(http_detail("<html>oops</html>"), "<html>oops</html>");
    }

    #[test]
    fn reads_usage_with_reasoning_detail() {
        let v: Value = serde_json::from_str(
            r#"{"usage":{"prompt_tokens":16,"completion_tokens":594,"completion_tokens_details":{"reasoning_tokens":588}}}"#,
        )
        .unwrap();
        assert_eq!(
            usage_of(&v),
            Some(Usage {
                prompt_tokens: 16,
                completion_tokens: 594,
                reasoning_tokens: 588
            })
        );
        assert_eq!(usage_of(&serde_json::json!({"usage": null})), None);
    }

    #[test]
    fn reply_folds_events_and_builds_the_message() {
        let mut reply = Reply::default();
        reply.take(&StreamEvent::Reasoning("hm".into()));
        reply.take(&StreamEvent::Text("Hi".into()));
        reply.take(&StreamEvent::Text(" there".into()));
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a"}),
        };
        reply.take(&StreamEvent::ToolCall(call.clone()));
        assert_eq!(
            reply.message(),
            Message::Assistant {
                content: "Hi there".into(),
                reasoning: Some("hm".into()),
                tool_calls: vec![call]
            }
        );
    }
}
