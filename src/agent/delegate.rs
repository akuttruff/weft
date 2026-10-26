//! The delegate: a tool that hands one self-contained piece of a task to a
//! fresh worker with the same tools and its own context. Its edits show on
//! the screen and ask for approval as usual; only its report comes back to
//! the worker that asked, so the files it read never land in that context.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::tandem::snapshot;
use super::worker::{Worker, WorkerError};
use super::{AgentEvent, Approver, Model, Yes, prompts};
use crate::lm::{Message, ToolSpec};
use crate::tools::{Ctx, Registry, Tool, ToolError, arg_str};

/// Tool rounds a helper may take.
pub const DELEGATE_ROUNDS: usize = 40;

pub struct Delegate {
    model: Box<dyn Model>,
    /// Every tool but `delegate` itself, so helpers do not spawn helpers.
    registry: Arc<Registry>,
}

impl Delegate {
    pub fn new(model: Box<dyn Model>, registry: Arc<Registry>) -> Self {
        Self { model, registry }
    }
}

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description:
                "Hand one self-contained part of the task to a helper with the same tools \
                          and its own context; it edits for real and reports back. It cannot see \
                          this conversation: say exactly what to do and what to report."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "What the helper should do and report, in full" }
                },
                "required": ["task"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let task = arg_str(args, "task")?;
        let approver: Arc<dyn Approver> = ctx.approver.clone().unwrap_or_else(|| Arc::new(Yes));

        // Everything the helper does shows on the screen except its answer
        // text, which comes back as this tool's result instead.
        let (events, mut inner) = mpsc::channel::<AgentEvent>(64);
        let outer = ctx.events.clone();
        let forward = tokio::spawn(async move {
            while let Some(evt) = inner.recv().await {
                if matches!(evt, AgentEvent::Text { .. }) {
                    continue;
                }
                if let Some(tx) = &outer {
                    let _ = tx.send(evt).await;
                }
            }
        });

        let mut worker = Worker::new(
            self.model.as_ref(),
            &self.registry,
            ctx,
            approver.as_ref(),
            events,
        )
        .rounds(DELEGATE_ROUNDS)
        .allowed(ctx.allowed.lock().unwrap().clone());
        let mut convo = vec![
            Message::System(prompts::WORKER.into()),
            Message::User(format!(
                "{}## Task\n{task}\n\n{}",
                snapshot(&ctx.root),
                prompts::DELEGATE_BRIEF
            )),
        ];
        let result = worker.run(&mut convo).await;
        // An `always` given to the helper holds for the rest of the session.
        ctx.allowed
            .lock()
            .unwrap()
            .extend(std::mem::take(&mut worker.allowed));
        drop(worker);
        let _ = forward.await;

        match result {
            Ok(out) => {
                let mut report = if out.answer.trim().is_empty() {
                    "(the helper had nothing to say)".to_string()
                } else {
                    out.answer
                };
                if !out.touched.is_empty() {
                    report.push_str(&format!(
                        "\n\nFiles the helper changed: {}",
                        out.touched.join(", ")
                    ));
                }
                Ok(report)
            }
            Err(WorkerError::Cancelled) => Err(ToolError::Failed("the helper was stopped".into())),
            Err(WorkerError::TooManyRounds(n)) => Err(ToolError::Failed(format!(
                "the helper took {n} rounds without finishing; give it a smaller piece"
            ))),
            Err(e @ WorkerError::ContextFull(_)) => Err(ToolError::Failed(format!(
                "the helper ran out of room: {e}"
            ))),
            Err(WorkerError::Model(e)) => {
                Err(ToolError::Failed(format!("the helper's model: {e}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lm::{LmError, Reply, Request, Sender, StreamEvent, ToolCall};
    use std::sync::Mutex;

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
    async fn a_helper_edits_in_the_open_and_reports_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let (events, mut rx) = mpsc::channel(64);
        let ctx = Ctx::new(dir.path().to_path_buf()).for_task(Some(events), Default::default());
        let model = Scripted(Mutex::new(vec![
            Reply {
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "edit_file".into(),
                    arguments: json!({"path": "a.txt", "old_string": "one", "new_string": "two"}),
                }],
                ..Default::default()
            },
            Reply {
                content: "Changed a.txt:1.".into(),
                ..Default::default()
            },
        ]));
        let delegate = Delegate::new(Box::new(model), Arc::new(Registry::builtin()));
        let report = delegate
            .run(&json!({"task": "change one to two in a.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            report,
            "Changed a.txt:1.\n\nFiles the helper changed: a.txt"
        );
        // `Yes` answers `always`, and that reaches the shared set.
        assert!(ctx.allowed.lock().unwrap().contains("edit_file"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "two\n"
        );

        drop(ctx);
        let mut kinds = Vec::new();
        while let Some(evt) = rx.recv().await {
            kinds.push(match evt {
                AgentEvent::ToolStart { .. } => "tool",
                AgentEvent::Text { .. } => "text",
                _ => "other",
            });
        }
        assert!(
            kinds.contains(&"tool"),
            "the helper's edit was shown: {kinds:?}"
        );
        assert!(!kinds.contains(&"text"), "its answer was not: {kinds:?}");
    }
}
