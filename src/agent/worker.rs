//! The tool loop: ask the model, run what it asks for, feed the results back,
//! until it answers in plain text.

use std::collections::HashSet;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{AgentEvent, Approver, Decision, Events, Model, Role, call_summary};
use crate::lm::{Message, Request, StreamEvent, Usage};
use crate::tools::{Ctx, Registry, ToolError};

/// Tool rounds before the loop gives up on a task.
pub const MAX_ROUNDS: usize = 60;
const MAX_TOKENS: u32 = 8_000;

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Model(#[from] crate::lm::LmError),
    #[error("stopped")]
    Cancelled,
    #[error("gave up after {0} tool rounds")]
    TooManyRounds(usize),
}

/// What a finished run amounts to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    /// The model's closing answer.
    pub answer: String,
    pub rounds: usize,
    pub usage: Usage,
    /// Files changed by edit_file or write_file, for the reviewer.
    pub touched: Vec<String>,
}

pub struct Worker<'a> {
    pub model: &'a dyn Model,
    pub registry: &'a Registry,
    pub ctx: &'a Ctx,
    pub approver: &'a dyn Approver,
    pub events: Events,
    pub cancel: CancellationToken,
    /// Tools approved for the whole session.
    pub allowed: HashSet<String>,
    /// Who is speaking, for the screen: the planner also runs this loop, read-only.
    pub role: Role,
    /// Tool rounds before giving up; see `MAX_ROUNDS`.
    pub max_rounds: usize,
}

impl Worker<'_> {
    /// Run until the model answers without calling a tool. `messages` is the
    /// conversation so far and is extended in place, so a follow-up can continue it.
    pub async fn run(&mut self, messages: &mut Vec<Message>) -> Result<Outcome, WorkerError> {
        let mut outcome = Outcome::default();
        let tools = self.registry.specs();

        for round in 1..=self.max_rounds {
            let req = Request {
                messages: messages.clone(),
                tools: tools.clone(),
                max_tokens: MAX_TOKENS,
            };
            let reply = self.turn(&req).await?;
            outcome.usage = add(outcome.usage, reply.usage);
            outcome.rounds = round;
            messages.push(reply.message());

            if reply.tool_calls.is_empty() {
                outcome.answer = reply.content.trim().to_string();
                return Ok(outcome);
            }

            for call in &reply.tool_calls {
                if self.cancel.is_cancelled() {
                    return Err(WorkerError::Cancelled);
                }
                let summary = call_summary(&call.name, &call.arguments);
                self.emit(AgentEvent::ToolStart {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    summary,
                    args: call.arguments.clone(),
                })
                .await;

                let result = self.run_tool(call).await;
                let (ok, output) = match result {
                    Ok(out) => (true, out),
                    Err(e) => (false, format!("error: {e}")),
                };
                if ok
                    && matches!(call.name.as_str(), "edit_file" | "write_file")
                    && let Some(path) = call.arguments.get("path").and_then(|v| v.as_str())
                    && !outcome.touched.iter().any(|p| p == path)
                {
                    outcome.touched.push(path.to_string());
                }
                self.emit(AgentEvent::ToolDone {
                    id: call.id.clone(),
                    ok,
                    output: output.clone(),
                })
                .await;
                messages.push(Message::Tool {
                    call_id: call.id.clone(),
                    content: output,
                });
            }
        }
        Err(WorkerError::TooManyRounds(self.max_rounds))
    }

    /// One model turn, forwarding its stream to the screen.
    async fn turn(&self, req: &Request) -> Result<crate::lm::Reply, WorkerError> {
        let role = self.role;
        self.emit(AgentEvent::TurnStart { role }).await;
        let began = Instant::now();
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        let events = self.events.clone();
        // Rust: a spawned task forwards the model's pieces while this task waits
        // on the model. The forwarder ends when `tx` (owned by the stream call
        // below) is dropped, which closes the channel.
        let forward = tokio::spawn(async move {
            while let Some(evt) = rx.recv().await {
                let out = match evt {
                    StreamEvent::Reasoning(text) => AgentEvent::Reasoning { role, text },
                    StreamEvent::Text(text) => AgentEvent::Text { role, text },
                    StreamEvent::ToolCall(_) => continue, // reported as ToolStart once it runs
                };
                if events.send(out).await.is_err() {
                    break;
                }
            }
        });

        let reply = tokio::select! {
            r = self.model.stream(req, &tx) => r,
            _ = self.cancel.cancelled() => Err(crate::lm::LmError::Stream("stopped".into())),
        };
        drop(tx);
        let _ = forward.await;

        let reply = match reply {
            Ok(r) => r,
            Err(_) if self.cancel.is_cancelled() => return Err(WorkerError::Cancelled),
            Err(e) => return Err(e.into()),
        };
        self.emit(AgentEvent::TurnEnd {
            role,
            usage: reply.usage,
            seconds: began.elapsed().as_secs_f32(),
        })
        .await;
        Ok(reply)
    }

    async fn run_tool(&mut self, call: &crate::lm::ToolCall) -> Result<String, ToolError> {
        if let Some(what) = self.registry.approval(call)
            && !self.allowed.contains(&call.name)
        {
            match self.approver.ask(&what).await {
                Decision::Once => {}
                Decision::Always => {
                    self.allowed.insert(call.name.clone());
                }
                Decision::No => return Err(ToolError::Refused("the user said no".into())),
            }
        }
        self.registry.run(call, self.ctx).await
    }

    async fn emit(&self, evt: AgentEvent) {
        let _ = self.events.send(evt).await;
    }
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Yes;
    use crate::lm::{LmError, Reply, Sender, ToolCall};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    /// A model that plays back scripted replies, one per turn.
    struct Scripted {
        replies: Mutex<Vec<Reply>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl Model for Scripted {
        async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
            self.seen.lock().unwrap().push(req.messages.clone());
            let reply = self.replies.lock().unwrap().remove(0);
            let _ = tx.send(StreamEvent::Text(reply.content.clone())).await;
            Ok(reply)
        }
        fn name(&self) -> &str {
            "scripted"
        }
    }

    fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn runs_tools_until_the_model_answers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let ctx = Ctx {
            root: dir.path().to_path_buf(),
        };
        let registry = Registry::builtin();
        let model = Scripted {
            replies: Mutex::new(vec![
                Reply {
                    content: "".into(),
                    tool_calls: vec![
                        call("c1", "read_file", json!({"path": "a.txt"})),
                        call(
                            "c2",
                            "edit_file",
                            json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
                        ),
                    ],
                    usage: Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        reasoning_tokens: 0,
                    },
                    ..Default::default()
                },
                Reply {
                    content: "Done.".into(),
                    usage: Usage {
                        prompt_tokens: 20,
                        completion_tokens: 2,
                        reasoning_tokens: 0,
                    },
                    ..Default::default()
                },
            ]),
            seen: Mutex::new(vec![]),
        };
        let (events, mut rx) = mpsc::channel(64);
        let mut worker = Worker {
            model: &model,
            registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            role: Role::Worker,
            max_rounds: MAX_ROUNDS,
        };
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("change it".into()),
        ];
        let outcome = worker.run(&mut messages).await.unwrap();

        assert_eq!(outcome.answer, "Done.");
        assert_eq!(outcome.rounds, 2);
        assert_eq!(outcome.usage.prompt_tokens, 30);
        assert_eq!(outcome.touched, vec!["a.txt"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "bye\n"
        );
        // The second turn saw the assistant's calls and both tool results.
        let seen = model.seen.lock().unwrap().clone();
        assert_eq!(seen[1].len(), 5);
        assert!(
            matches!(&seen[1][3], Message::Tool { call_id, content } if call_id == "c1" && content.starts_with("1\thello"))
        );
        assert!(
            matches!(&seen[1][4], Message::Tool { content, .. } if content == "edited a.txt:1 (+1 −1)")
        );
        assert_eq!(messages.len(), 6, "the conversation was extended in place");
        assert!(
            worker.allowed.contains("edit_file"),
            "'always' was remembered"
        );

        drop(worker);
        let mut kinds = Vec::new();
        while let Some(evt) = rx.recv().await {
            kinds.push(match evt {
                AgentEvent::TurnStart { .. } => "start",
                AgentEvent::Text { .. } => "text",
                AgentEvent::ToolStart { .. } => "tool",
                AgentEvent::ToolDone { ok: true, .. } => "ok",
                AgentEvent::ToolDone { ok: false, .. } => "fail",
                AgentEvent::TurnEnd { .. } => "end",
                AgentEvent::Reasoning { .. } => "think",
                AgentEvent::Phase { .. } | AgentEvent::Plan { .. } | AgentEvent::Review { .. } => {
                    "other"
                }
            });
        }
        assert_eq!(
            kinds,
            [
                "start", "text", "end", "tool", "ok", "tool", "ok", "start", "text", "end"
            ]
        );
    }

    #[tokio::test]
    async fn a_refusal_goes_back_to_the_model_as_an_error() {
        struct Never;
        #[async_trait]
        impl Approver for Never {
            async fn ask(&self, _: &str) -> Decision {
                Decision::No
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            root: dir.path().to_path_buf(),
        };
        let registry = Registry::builtin();
        let model = Scripted {
            replies: Mutex::new(vec![
                Reply {
                    tool_calls: vec![call("c1", "bash", json!({"command": "echo hi"}))],
                    ..Default::default()
                },
                Reply {
                    content: "ok".into(),
                    ..Default::default()
                },
            ]),
            seen: Mutex::new(vec![]),
        };
        let (events, _rx) = mpsc::channel(64);
        let mut worker = Worker {
            model: &model,
            registry: &registry,
            ctx: &ctx,
            approver: &Never,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            role: Role::Worker,
            max_rounds: MAX_ROUNDS,
        };
        let mut messages = vec![Message::User("run it".into())];
        worker.run(&mut messages).await.unwrap();
        assert!(
            matches!(&messages[2], Message::Tool { content, .. } if content == "error: the user said no")
        );
    }
}
