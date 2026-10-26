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
/// Room left under the window for the answer and the next tool results.
const MARGIN: u32 = 4_000;
/// How many of the latest tool results stay when old ones are dropped.
const KEEP_RESULTS: usize = 3;
/// How many of the latest messages stay verbatim when a task is folded.
const KEEP_MESSAGES: usize = 4;
/// What the text of an old edit or new file is replaced with.
pub const OMITTED: &str = "(omitted to make room; the file on disk has it)";
/// What an old tool result is replaced with.
pub const DROPPED: &str =
    "(dropped to make room in the context; call the tool again if you need it)";

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Model(#[from] crate::lm::LmError),
    #[error("stopped")]
    Cancelled,
    #[error("gave up after {0} tool rounds")]
    TooManyRounds(usize),
    #[error(
        "the conversation no longer fits the model's window ({0} tokens); what was done so far is kept; /compact or /clear, then carry on"
    )]
    ContextFull(u32),
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
    /// The model's context window in tokens; 0 switches the guard off.
    pub window: u32,
    /// A small model that can fold this task's earlier rounds into a note
    /// when dropping old results is not enough. None: the task stops instead.
    pub digest: Option<&'a dyn Model>,
}

impl<'a> Worker<'a> {
    /// A worker with the usual settings: the task's stop signal from `ctx`, no
    /// approvals yet, the worker's role, the usual rounds and window.
    pub fn new(
        model: &'a dyn Model,
        registry: &'a Registry,
        ctx: &'a Ctx,
        approver: &'a dyn Approver,
        events: Events,
    ) -> Self {
        Self {
            model,
            registry,
            ctx,
            approver,
            events,
            cancel: ctx.cancel.clone(),
            allowed: HashSet::new(),
            role: Role::Worker,
            max_rounds: MAX_ROUNDS,
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        }
    }

    /// The model that folds a long task's earlier rounds into a note.
    pub fn digest(mut self, digest: Option<&'a dyn Model>) -> Self {
        self.digest = digest;
        self
    }

    // Rust: setters that take `self` by value and give it back let the caller
    // chain them: `Worker::new(..).role(Role::Scout).rounds(8)`.
    pub fn role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    pub fn rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// The context window in tokens; 0 switches the in-task guard off.
    pub fn window(mut self, window: u32) -> Self {
        self.window = window;
        self
    }

    /// Tools already approved for the session.
    pub fn allowed(mut self, allowed: HashSet<String>) -> Self {
        self.allowed = allowed;
        self
    }
}

impl Worker<'_> {
    /// Run until the model answers without calling a tool. `messages` is the
    /// conversation so far and is extended in place, so a follow-up can continue it.
    pub async fn run(&mut self, messages: &mut Vec<Message>) -> Result<Outcome, WorkerError> {
        let mut outcome = Outcome::default();
        let tools = self.registry.specs();
        // Where this run's own rounds begin; a fold keeps what came before.
        let own_rounds_from = messages.len();

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

            // Near the window, old tool results and the file contents of old
            // edits go first: the model has seen them, and the disk has them.
            // When nothing is left to drop, the task's earlier rounds are
            // folded into a progress note; without a model for that, the task
            // stops rather than failing on the server.
            let ceiling = self.window.saturating_sub(MAX_TOKENS + MARGIN);
            if self.window > 0 && reply.usage.prompt_tokens >= ceiling {
                let before = reply.usage.prompt_tokens;
                let mut dropped = drop_old_results(messages, KEEP_RESULTS)
                    + shrink_old_calls(messages, KEEP_RESULTS);
                if dropped == 0 {
                    // Still too big with the last few kept: keep only the newest.
                    dropped = drop_old_results(messages, 1) + shrink_old_calls(messages, 1);
                }
                if dropped > 0 {
                    self.emit(AgentEvent::Trimmed {
                        dropped,
                        tokens: before,
                    })
                    .await;
                    continue;
                }
                // Rust: the fold runs a worker of its own, so `run` would
                // contain itself; boxing the inner future breaks the cycle.
                let folded = match self.digest {
                    Some(digest) => {
                        Box::pin(super::compact::fold_task(
                            digest,
                            self.ctx,
                            self.events.clone(),
                            self.cancel.clone(),
                            messages,
                            own_rounds_from,
                            KEEP_MESSAGES,
                        ))
                        .await?
                    }
                    None => None,
                };
                match folded {
                    Some(after) => {
                        self.emit(AgentEvent::Compacted {
                            before,
                            after,
                            tasks: 0,
                        })
                        .await
                    }
                    None => return Err(WorkerError::ContextFull(before)),
                }
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
        // `always` answers live in two places: this worker's own set, and the
        // task's shared set, which a helper's worker reads and writes too, so
        // an approval given to either holds for both.
        let shared = |name: &str| self.ctx.allowed.lock().unwrap().contains(name);
        if let Some(what) = self.registry.approval(call)
            && !self.allowed.contains(&call.name)
            && !shared(&call.name)
        {
            match self.approver.ask(&what).await {
                Decision::Once => {}
                Decision::Always => {
                    self.allowed.insert(call.name.clone());
                    self.ctx.allowed.lock().unwrap().insert(call.name.clone());
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

/// Replace every tool result but the last `keep` with a stub; how many went.
/// Short results stay: they cost little and dropping them would only confuse.
pub fn drop_old_results(messages: &mut [Message], keep: usize) -> usize {
    let total = messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .count();
    let mut seen = 0;
    let mut dropped = 0;
    for m in messages.iter_mut() {
        if let Message::Tool { content, .. } = m {
            seen += 1;
            if seen + keep <= total && content.len() > 200 && content != DROPPED {
                *content = DROPPED.to_string();
                dropped += 1;
            }
        }
    }
    dropped
}

/// Blank the file text inside old `edit_file` and `write_file` calls, all but
/// the last `keep` assistant turns with calls; how many went. The screen has
/// shown the diff and the disk holds the result, so the model loses nothing.
pub fn shrink_old_calls(messages: &mut [Message], keep: usize) -> usize {
    let total = messages
        .iter()
        .filter(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
        .count();
    let mut seen = 0;
    let mut shrunk = 0;
    for m in messages.iter_mut() {
        let Message::Assistant { tool_calls, .. } = m else {
            continue;
        };
        if tool_calls.is_empty() {
            continue;
        }
        seen += 1;
        if seen + keep > total {
            continue;
        }
        for call in tool_calls.iter_mut() {
            let Some(args) = call.arguments.as_object_mut() else {
                continue;
            };
            for key in ["content", "new_string", "old_string"] {
                if let Some(v) = args.get_mut(key)
                    && v.as_str().is_some_and(|s| s.len() > 200)
                {
                    *v = serde_json::Value::String(OMITTED.into());
                    shrunk += 1;
                }
            }
        }
    }
    shrunk
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
        let ctx = Ctx::new(dir.path().to_path_buf());
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
        let mut worker = Worker::new(&model, &registry, &ctx, &Yes, events).window(0);
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
                AgentEvent::Trimmed { .. } => "trimmed",
                AgentEvent::Hook { .. } => "hook",
                AgentEvent::Step { .. } => "step",
                AgentEvent::Compacted { .. } => "compacted",
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
        let ctx = Ctx::new(dir.path().to_path_buf());
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
        let mut worker = Worker::new(&model, &registry, &ctx, &Never, events).window(0);
        let mut messages = vec![Message::User("run it".into())];
        worker.run(&mut messages).await.unwrap();
        assert!(
            matches!(&messages[2], Message::Tool { content, .. } if content == "error: the user said no")
        );
    }
    #[tokio::test]
    async fn near_the_window_old_results_are_dropped_then_the_task_stops() {
        let dir = tempfile::tempdir().unwrap();
        // A small file: its results are too short to be worth dropping.
        std::fs::write(dir.path().join("a.txt"), "x".repeat(10)).unwrap();
        let registry = Registry::builtin();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let big = |n: u32| Reply {
            tool_calls: vec![call("c", "read_file", json!({"path": "a.txt"}))],
            usage: Usage {
                prompt_tokens: n,
                completion_tokens: 1,
                reasoning_tokens: 0,
            },
            ..Default::default()
        };
        // Every turn claims to be near the window: 20k of a 30k window with a 12k margin.
        let model = Scripted {
            replies: Mutex::new(vec![big(20_000), big(20_000)]),
            seen: Mutex::new(vec![]),
        };
        let (events, mut rx) = mpsc::channel(64);
        let mut worker = Worker::new(&model, &registry, &ctx, &Yes, events).window(30_000);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("t".into()),
            Message::Tool {
                call_id: "old".into(),
                content: "y".repeat(500),
            },
        ];
        let err = worker.run(&mut messages).await.unwrap_err();
        assert!(matches!(err, WorkerError::ContextFull(20_000)), "{err}");
        // The old result went after the first turn; after the second there
        // was nothing left worth dropping, so the task stopped.
        assert!(matches!(&messages[2], Message::Tool { content, .. } if content == DROPPED));
        let stubs = messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { content, .. } if content == DROPPED))
            .count();
        let kept = messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { content, .. } if content != DROPPED))
            .count();
        assert_eq!((stubs, kept), (1, 2));
        drop(worker);
        let mut trimmed = 0;
        while let Some(evt) = rx.recv().await {
            if matches!(evt, AgentEvent::Trimmed { .. }) {
                trimmed += 1;
            }
        }
        assert_eq!(trimmed, 1);
    }
    #[tokio::test]
    async fn an_approval_shared_through_the_context_is_not_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let registry = Registry::builtin();
        let ctx = Ctx::new(dir.path().to_path_buf());
        ctx.allowed.lock().unwrap().insert("edit_file".into());
        let model = Scripted {
            replies: Mutex::new(vec![
                Reply {
                    tool_calls: vec![call(
                        "c1",
                        "edit_file",
                        json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
                    )],
                    ..Default::default()
                },
                Reply {
                    content: "Done.".into(),
                    ..Default::default()
                },
            ]),
            seen: Mutex::new(vec![]),
        };
        struct Refuses;
        #[async_trait]
        impl Approver for Refuses {
            async fn ask(&self, _what: &str) -> Decision {
                Decision::No
            }
        }
        let (events, _rx) = mpsc::channel(64);
        // The approver would refuse if asked; it is not asked.
        let mut worker = Worker::new(&model, &registry, &ctx, &Refuses, events).window(0);
        let mut messages = vec![Message::User("change it".into())];
        let out = worker.run(&mut messages).await.unwrap();
        assert_eq!(out.answer, "Done.");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "bye\n"
        );
    }
    #[tokio::test]
    async fn when_nothing_can_be_dropped_the_task_is_folded_and_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let registry = Registry::builtin();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let big = |n: u32| Reply {
            tool_calls: vec![call("c", "read_file", json!({"path": "a.txt"}))],
            usage: Usage {
                prompt_tokens: n,
                completion_tokens: 1,
                reasoning_tokens: 0,
            },
            ..Default::default()
        };
        // Three small rounds whose results are too short to drop, then one
        // that reports the window nearly full, then the answer.
        let model = Scripted {
            replies: Mutex::new(vec![
                big(1_000),
                big(1_000),
                big(1_000),
                big(20_000),
                Reply {
                    content: "Finished.".into(),
                    ..Default::default()
                },
            ]),
            seen: Mutex::new(vec![]),
        };
        let digest = Scripted {
            replies: Mutex::new(vec![Reply {
                content: "- a.txt read a few times; nothing changed yet.".into(),
                ..Default::default()
            }]),
            seen: Mutex::new(vec![]),
        };
        let (events, mut rx) = mpsc::channel(64);
        let mut worker = Worker::new(&model, &registry, &ctx, &Yes, events)
            .window(30_000)
            .digest(Some(&digest));
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("look around".into()),
        ];
        let out = worker.run(&mut messages).await.unwrap();
        assert_eq!(out.answer, "Finished.");
        let folded = messages
            .iter()
            .position(|m| matches!(m, Message::User(s) if s.starts_with("## Progress so far")))
            .expect("a progress note replaced the early rounds");
        assert_eq!(folded, 2, "right after the task's own message");
        assert!(matches!(&messages[1], Message::User(s) if s == "look around"));
        // The digest saw the early rounds, and the screen heard about the fold.
        assert!(format!("{:?}", digest.seen.lock().unwrap()[0][1]).contains("read_file"));
        drop(worker);
        let mut folds = 0;
        while let Some(evt) = rx.recv().await {
            if matches!(evt, AgentEvent::Compacted { tasks: 0, .. }) {
                folds += 1;
            }
        }
        assert_eq!(folds, 1);
    }

    #[test]
    fn old_file_texts_in_calls_are_blanked_but_the_newest_stay() {
        let edit = |n: usize| Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![call(
                &format!("e{n}"),
                "write_file",
                json!({"path": format!("f{n}.rs"), "content": "y".repeat(500)}),
            )],
        };
        let mut messages = vec![edit(1), edit(2), edit(3), edit(4)];
        assert_eq!(shrink_old_calls(&mut messages, 3), 1);
        assert_eq!(shrink_old_calls(&mut messages, 1), 2);
        let texts: Vec<String> = messages
            .iter()
            .map(|m| match m {
                Message::Assistant { tool_calls, .. } => tool_calls[0].arguments["content"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert!(texts[..3].iter().all(|t| t == OMITTED));
        assert_eq!(texts[3].len(), 500, "the newest call keeps its text");
    }
}
