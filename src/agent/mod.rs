//! The agent: a worker that runs the tool loop, and the events it reports to
//! whatever is drawing the screen.

pub mod prompts;
pub mod scout;
pub mod tandem;
pub mod worker;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::lm::{LmError, Reply, Request, Sender, Usage};

/// Which model is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Planner,
    Worker,
    /// The scout: a read-only helper the other two can ask.
    Scout,
}

/// A model, whichever route it answers on. The worker loop only needs this.
// Rust: both `ChatModel` and `RawModel` get this trait, so the loop can hold a
// `Box<dyn Model>` and tests can hand it a scripted fake instead.
#[async_trait]
pub trait Model: Send + Sync {
    async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError>;
    fn name(&self) -> &str;
}

#[async_trait]
impl Model for ChatModel {
    async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
        ChatModel::stream(self, req, tx).await
    }
    fn name(&self) -> &str {
        &self.endpoint.model
    }
}

#[async_trait]
impl Model for RawModel {
    async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
        RawModel::stream(self, req, tx).await
    }
    fn name(&self) -> &str {
        &self.endpoint.model
    }
}

/// What the screen hears while the agent works.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A model has started a turn.
    TurnStart {
        role: Role,
    },
    Reasoning {
        role: Role,
        text: String,
    },
    Text {
        role: Role,
        text: String,
    },
    /// A tool is about to run (or is waiting for approval). `args` are the
    /// call's arguments, so the screen can show an edit as a diff.
    ToolStart {
        id: String,
        name: String,
        summary: String,
        args: serde_json::Value,
    },
    /// The tool finished; `output` is what the model will see.
    ToolDone {
        id: String,
        ok: bool,
        output: String,
    },
    /// The tandem moved on: planning, working, reviewing, fixing.
    Phase {
        name: String,
    },
    /// The planner's plan, whole.
    Plan {
        text: String,
    },
    /// The reviewer's verdict on the diff.
    Review {
        approved: bool,
        text: String,
    },
    /// A model finished a turn and this is what it cost.
    TurnEnd {
        role: Role,
        usage: Usage,
        seconds: f32,
    },
}

pub type Events = mpsc::Sender<AgentEvent>;

/// The answer to "may this tool run?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Once,
    /// Every call of this tool for the rest of the session.
    Always,
    No,
}

/// Whoever answers approval questions: the terminal, or a test.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn ask(&self, what: &str) -> Decision;
}

/// Approves everything. For tests and `--yes`.
pub struct Yes;

#[async_trait]
impl Approver for Yes {
    async fn ask(&self, _what: &str) -> Decision {
        Decision::Always
    }
}

/// A one-line description of a tool call for the transcript, e.g. `Read src/a.rs:1-120`.
pub fn call_summary(name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| {
        args.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    let n = |k: &str| args.get(k).and_then(serde_json::Value::as_u64);
    match name {
        "read_file" => match (n("start"), n("end")) {
            (Some(a), Some(b)) => format!("Read {}:{a}-{b}", s("path")),
            (Some(a), None) => format!("Read {}:{a}-", s("path")),
            _ => format!("Read {}", s("path")),
        },
        "list_files" => format!(
            "List {}",
            if s("path").is_empty() { "." } else { s("path") }
        ),
        "search" => {
            let mut out = format!("Search /{}/", s("pattern"));
            if !s("glob").is_empty() {
                out.push_str(&format!(" in {}", s("glob")));
            } else if !s("path").is_empty() {
                out.push_str(&format!(" in {}", s("path")));
            }
            out
        }
        "scout" => format!("Scout {}", s("question")),
        "edit_file" => format!("Edit {}", s("path")),
        "write_file" => format!("Write {}", s("path")),
        "bash" => format!("Bash {}", s("command").lines().next().unwrap_or("")),
        // `server__tool` from an MCP server: the two names and a glimpse of the arguments.
        other => {
            let shown = match other.split_once("__") {
                Some((server, tool)) => format!("{server} {tool}"),
                None => other.to_string(),
            };
            let brief: String = args.to_string().chars().take(80).collect();
            if brief == "{}" || brief == "null" {
                shown
            } else {
                format!("{shown} {brief}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summaries_read_like_claude_code() {
        assert_eq!(
            call_summary(
                "read_file",
                &json!({"path": "a.rs", "start": 1, "end": 120})
            ),
            "Read a.rs:1-120"
        );
        assert_eq!(
            call_summary("read_file", &json!({"path": "a.rs"})),
            "Read a.rs"
        );
        assert_eq!(
            call_summary("search", &json!({"pattern": "fn main", "glob": "*.rs"})),
            "Search /fn main/ in *.rs"
        );
        assert_eq!(
            call_summary("bash", &json!({"command": "cargo test\necho"})),
            "Bash cargo test"
        );
        assert_eq!(call_summary("list_files", &json!({})), "List .");
        assert_eq!(
            call_summary("deep-research__search", &json!({"query": "rmcp crate"})),
            "deep-research search {\"query\":\"rmcp crate\"}"
        );
        assert_eq!(
            call_summary("deep-research__status", &json!({})),
            "deep-research status"
        );
    }
}
