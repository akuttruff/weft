//! The scout: a tool that answers one question about the code by running a
//! short read-only loop on a cheaper model. The worker and planner call it
//! like any other tool; the files it reads stay in its own context, and only
//! its answer (paths, line numbers, a few sentences) comes back.

use std::path::Path;

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

        let mut worker = Worker::new(self.model.as_ref(), &registry, ctx, &Yes, events)
            .role(Role::Scout)
            .rounds(SCOUT_ROUNDS);
        let mut convo = vec![
            Message::System(prompts::SCOUT.into()),
            Message::User(format!("{}## Question\n{question}", snapshot(&ctx.root))),
        ];
        let mut result = worker.run(&mut convo).await;

        // A small model will sometimes cite a file it never opened. Paths in
        // the answer are checked against the folder, and it gets one chance to
        // look again before the asker sees the answer.
        if let Ok(out) = &result {
            let missing = missing_paths(&out.answer, &ctx.root);
            if !missing.is_empty() {
                convo.push(Message::User(format!(
                    "These paths are not in the project: {}. Search for what you meant and answer again, citing only files you have actually opened.",
                    missing.join(", ")
                )));
                result = worker.run(&mut convo).await;
            }
        }
        drop(worker);
        let _ = forward.await;

        match result {
            Ok(out) if out.answer.trim().is_empty() => Ok("(the scout had nothing to say)".into()),
            Ok(out) => {
                let missing = missing_paths(&out.answer, &ctx.root);
                if missing.is_empty() {
                    Ok(out.answer)
                } else {
                    Ok(format!(
                        "{}\n\n(weft: these cited paths do not exist: {})",
                        out.answer,
                        missing.join(", ")
                    ))
                }
            }
            Err(WorkerError::Cancelled) => Err(ToolError::Failed("the scout was stopped".into())),
            Err(WorkerError::TooManyRounds(n)) => Err(ToolError::Failed(format!(
                "the scout looked {n} times without reaching an answer; ask something narrower or search yourself"
            ))),
            Err(WorkerError::Model(e)) => Err(ToolError::Failed(format!("the scout's model: {e}"))),
            Err(e @ WorkerError::ContextFull(_)) => {
                Err(ToolError::Failed(format!("the scout ran out of room: {e}")))
            }
        }
    }
}

/// File extensions a cited path may carry; a bare word with a dot in it
/// (`self.model`, `e.g.`) is prose, not a path.
const CODE_EXTS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "rb", "go", "java", "kt", "swift", "c",
    "h", "cpp", "hpp", "cs", "md", "toml", "json", "yaml", "yml", "html", "css", "scss", "sh",
    "sql", "txt", "lock", "jinja", "vue", "svelte", "php", "ex", "exs", "zig",
];

/// The paths an answer cites that are not in the project: `src/nope.rs` from
/// `src/nope.rs:12-20`. Only things that look like paths are checked: a token
/// with a code extension, or a slash-path whose first folder exists.
pub fn missing_paths(answer: &str, root: &Path) -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    let tokens = answer.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '(' | ')' | '[' | ']' | '"' | '\'' | ',' | ';' | '*'
            )
    });
    for raw in tokens {
        let Some(path) = cited_path(raw) else {
            continue;
        };
        let looks_like_path = has_code_ext(path)
            || (path.contains('/')
                && path
                    .split('/')
                    .next()
                    .is_some_and(|first| !first.is_empty() && root.join(first).is_dir()));
        if looks_like_path && !root.join(path).exists() && !missing.iter().any(|m| m == path) {
            missing.push(path.to_string());
        }
    }
    missing
}

/// `src/a.rs:12-20.` → `src/a.rs`; None for URLs, Rust paths and calls.
fn cited_path(token: &str) -> Option<&str> {
    if token.starts_with("http") || token.contains("::") || token.contains('{') {
        return None;
    }
    let mut t = token.trim_end_matches(['.', ':']);
    // Line references: `:12`, `:12-20`, `:12–20`, `:12:5`.
    while let Some((head, tail)) = t.rsplit_once(':') {
        let is_lines = !tail.is_empty()
            && tail.starts_with(|c: char| c.is_ascii_digit())
            && tail
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | '–'));
        if !is_lines {
            break;
        }
        t = head.trim_end_matches(['.', ':']);
    }
    let t = t.trim_start_matches("./");
    if t.is_empty() || t.starts_with('/') || t.starts_with('~') {
        return None;
    }
    Some(t)
}

fn has_code_ext(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && CODE_EXTS.contains(&ext),
        None => false,
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

    #[tokio::test]
    async fn a_guessed_path_gets_one_more_look() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/view.rs"), "fn draw() {}\n").unwrap();
        let (events, _rx) = mpsc::channel(64);
        let ctx = Ctx::new(dir.path().to_path_buf()).for_task(Some(events), Default::default());
        let scout = Scout::new(Box::new(Scripted(Mutex::new(vec![
            Reply {
                content: "`src/model/edits.rs:3` holds EditLine.".into(),
                ..Default::default()
            },
            Reply {
                content: "`src/view.rs:1` draws it.".into(),
                ..Default::default()
            },
        ]))));
        let out = scout
            .run(&json!({"question": "where is the diff drawn?"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "`src/view.rs:1` draws it.");
    }

    #[test]
    fn only_real_looking_paths_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/ui")).unwrap();
        std::fs::write(dir.path().join("src/ui/diff.rs"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let root = dir.path();
        let answer = "See `src/ui/diff.rs:30-49` and (src/ui/view.rs:150–151), plus Cargo.toml. \
                      The `self.model` field, e.g. `Kind::Removed`, red/green rows, ./src/ui, \
                      https://example.com/a.rs and `src/model/edits.rs:12:5`.";
        assert_eq!(
            missing_paths(answer, root),
            ["src/ui/view.rs", "src/model/edits.rs"]
        );
        assert!(missing_paths("nothing cited here.", root).is_empty());
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
