//! The scout: a tool that answers one question about the code by running a
//! short read-only loop on a cheaper model. The worker and planner call it
//! like any other tool; the files it reads stay in its own context, and only
//! its answer (paths, line numbers, a few sentences) comes back.

use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::tandem::snapshot;
use super::worker::{Worker, WorkerError};
use super::{AgentEvent, Model, Role, Yes, prompts};
use crate::lm::{Message, ToolSpec};
use crate::tools::{Ctx, Registry, Tool, ToolError, arg_str};

/// Tool rounds the scout may take before it must answer.
pub const SCOUT_ROUNDS: usize = 8;

pub struct Scout {
    model: Box<dyn Model>,
}

impl Scout {
    pub fn new(model: Box<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Tool for Scout {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "scout".into(),
            description: "Ask one question about where something lives or how it works in this \
                          project. A second model searches and reads the code and answers with \
                          paths, line numbers and a short explanation, never file contents. Use \
                          it before reading files when you do not know where to look; then read \
                          the exact spot yourself. One specific question per call."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, in one or two sentences" }
                },
                "required": ["question"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let question = arg_str(args, "question")?;
        let registry = Registry::read_only();

        // Rust: the scout's own events go to a small channel; a forwarding
        // task passes on only what the screen should know (turn starts and
        // costs), so the scout's searches never show as transcript lines.
        let (events, mut inner) = mpsc::channel::<AgentEvent>(64);
        let outer = ctx.events.clone();
        let forward = tokio::spawn(async move {
            while let Some(evt) = inner.recv().await {
                let pass = matches!(
                    evt,
                    AgentEvent::TurnStart { .. }
                        | AgentEvent::Reasoning { .. }
                        | AgentEvent::TurnEnd { .. }
                );
                if pass && let Some(tx) = &outer {
                    let _ = tx.send(evt).await;
                }
            }
        });

        let mut worker = Worker {
            model: self.model.as_ref(),
            registry: &registry,
            ctx,
            approver: &Yes,
            events,
            cancel: ctx.cancel.clone(),
            allowed: HashSet::new(),
            role: Role::Scout,
            max_rounds: SCOUT_ROUNDS,
        };
        let mut convo = vec![
            Message::System(prompts::SCOUT.into()),
            Message::User(format!("{}## Question\n{question}", snapshot(&ctx.root))),
        ];
        let result = worker.run(&mut convo).await;
        drop(worker);
        let _ = forward.await;

        match result {
            Ok(out) if out.answer.trim().is_empty() => Ok("(the scout had nothing to say)".into()),
            Ok(out) => Ok(out.answer),
            Err(WorkerError::Cancelled) => Err(ToolError::Failed("the scout was stopped".into())),
            Err(WorkerError::TooManyRounds(n)) => Err(ToolError::Failed(format!(
                "the scout looked {n} times without reaching an answer; ask something narrower or search yourself"
            ))),
            Err(WorkerError::Model(e)) => Err(ToolError::Failed(format!("the scout's model: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lm::{LmError, Reply, Request, Sender, StreamEvent, ToolCall, Usage};
    use std::sync::Mutex;

    /// Plays back one reply per turn.
    struct Scripted(Mutex<Vec<Reply>>);

    #[async_trait]
    impl Model for Scripted {
        async fn stream(&self, _req: &Request, tx: &Sender) -> Result<Reply, LmError> {
            let reply = self.0.lock().unwrap().remove(0);
            let _ = tx.send(StreamEvent::Text(reply.content.clone())).await;
            Ok(reply)
        }
        fn name(&self) -> &str {
            "scripted"
        }
    }

    #[tokio::test]
    async fn looks_then_answers_and_reports_only_its_cost() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn health() {}\n").unwrap();
        let (events, mut rx) = mpsc::channel(64);
        let ctx = Ctx::new(dir.path().to_path_buf()).for_task(Some(events), Default::default());
        let scout = Scout::new(Box::new(Scripted(Mutex::new(vec![
            Reply {
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "search".into(),
                    arguments: json!({"pattern": "fn health"}),
                }],
                usage: Usage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    reasoning_tokens: 0,
                },
                ..Default::default()
            },
            Reply {
                content: "`a.rs:1` defines health().".into(),
                ..Default::default()
            },
        ]))));

        let out = scout
            .run(&json!({"question": "where is health defined?"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "`a.rs:1` defines health().");

        drop(ctx);
        let mut kinds = Vec::new();
        while let Some(evt) = rx.recv().await {
            kinds.push(match evt {
                AgentEvent::TurnStart { role } => format!("start:{role:?}"),
                AgentEvent::TurnEnd { role, .. } => format!("end:{role:?}"),
                AgentEvent::ToolStart { .. } | AgentEvent::ToolDone { .. } => "tool".into(),
                _ => "other".into(),
            });
        }
        // Two turns, no tool lines: the search stayed inside the scout.
        assert_eq!(
            kinds,
            ["start:Scout", "end:Scout", "start:Scout", "end:Scout"]
        );
    }

    #[test]
    fn the_spec_asks_for_a_question() {
        let scout = Scout::new(Box::new(Scripted(Mutex::new(vec![]))));
        let spec = scout.spec();
        assert_eq!(spec.name, "scout");
        assert_eq!(spec.parameters["required"], json!(["question"]));
        assert!(scout.approval(&json!({})).is_none());
    }
}
