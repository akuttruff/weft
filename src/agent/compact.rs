//! Compaction: when the worker's conversation nears the context window, the
//! earlier tasks are folded into a handover note written by a small model,
//! and the last task stays verbatim. The worker then carries on from the
//! note, the way a colleague would from a good summary.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::convo::task_of;
use super::worker::{Worker, WorkerError};
use super::{AgentEvent, Model, Role, Yes, prompts};
use crate::lm::Message;
use crate::tools::{Ctx, Registry};

/// How much of each message the summariser is shown, in characters.
const TOOL_RESULT_CHARS: usize = 400;
const TEXT_CHARS: usize = 3_000;
/// The whole rendering is cut here: a small model's window is small.
const RENDER_CHARS: usize = 60_000;

/// What compaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    /// The worker's context before, in tokens, as its last turn reported.
    pub before: u32,
    /// A rough size after: the new conversation at four characters a token.
    pub after: u32,
    /// How many tasks were folded into the note.
    pub tasks: usize,
}

impl Compacted {
    pub fn event(self) -> AgentEvent {
        AgentEvent::Compacted {
            before: self.before,
            after: self.after,
            tasks: self.tasks,
        }
    }
}

/// Rewrite `messages` (system prompt first) as system prompt, handover note,
/// and the last task verbatim. `before` is only carried into the report.
pub async fn compact(
    model: &dyn Model,
    ctx: &Ctx,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    messages: &mut Vec<Message>,
    before: u32,
) -> Result<Compacted, WorkerError> {
    let (system, rest) = match messages.split_first() {
        Some((s @ Message::System(_), rest)) => (s.clone(), rest),
        _ => (Message::System(prompts::WORKER.into()), &messages[..]),
    };
    let (old, kept) = split(rest);
    let tasks = old.iter().filter_map(task_text).count();
    if old.is_empty() {
        return Ok(Compacted {
            before,
            after: estimate(messages),
            tasks: 0,
        });
    }

    let empty = Registry::empty();
    let mut writer = Worker::new(model, &empty, ctx, &Yes, events)
        .role(Role::Scout)
        .rounds(1);
    writer.cancel = cancel;
    let mut convo = vec![
        Message::System(prompts::COMPACT.into()),
        Message::User(render(old)),
    ];
    let note = writer.run(&mut convo).await?.answer;

    let mut fresh = vec![
        system,
        Message::User(format!(
            "## Earlier in this session\nThe conversation so far was condensed into this note; the files it mentions may need reading again.\n\n{}",
            note.trim()
        )),
        Message::Assistant {
            content: "Noted. I will go on from here.".into(),
            reasoning: None,
            tool_calls: vec![],
        },
    ];
    fresh.extend(kept.iter().cloned());
    let after = estimate(&fresh);
    *messages = fresh;
    Ok(Compacted {
        before,
        after,
        tasks,
    })
}

/// Mid-task: fold the rounds of a task from `from` up to the last `keep`
/// messages into a progress note, so the worker can carry on with room.
/// `None` when there is too little to fold. The task's own message, before
/// `from`, stays as it was.
pub async fn fold_task(
    model: &dyn Model,
    ctx: &Ctx,
    events: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
    messages: &mut Vec<Message>,
    from: usize,
    keep: usize,
) -> Result<Option<u32>, WorkerError> {
    let mut end = messages.len().saturating_sub(keep);
    // Never cut between a call and its result: the kept tail starts at a turn.
    while end > from && matches!(messages.get(end), Some(Message::Tool { .. })) {
        end -= 1;
    }
    if end < from + 4 {
        return Ok(None);
    }
    let old: Vec<Message> = messages[from..end].to_vec();

    let empty = Registry::empty();
    let mut writer = Worker::new(model, &empty, ctx, &Yes, events)
        .role(Role::Scout)
        .rounds(1);
    writer.cancel = cancel;
    let mut convo = vec![
        Message::System(prompts::COMPACT_TASK.into()),
        Message::User(render(&old)),
    ];
    let note = writer.run(&mut convo).await?.answer;

    let replacement = vec![
        Message::User(format!(
            "## Progress so far on this task\nThe earlier steps were condensed into this note; read a file again before editing it.\n\n{}",
            note.trim()
        )),
        Message::Assistant {
            content: "Noted. Carrying on.".into(),
            reasoning: None,
            tool_calls: vec![],
        },
    ];
    messages.splice(from..end, replacement);
    Ok(Some(estimate(messages)))
}

/// Everything before the last task, and the last task itself. A single task
/// is all "old": there is nothing newer to keep.
pub fn split(messages: &[Message]) -> (&[Message], &[Message]) {
    let last_task = messages
        .iter()
        .rposition(|m| task_text(m).is_some())
        .unwrap_or(0);
    if last_task == 0 {
        (messages, &[])
    } else {
        messages.split_at(last_task)
    }
}

fn task_text(m: &Message) -> Option<String> {
    match m {
        Message::User(s) => task_of(s),
        _ => None,
    }
}

/// The old messages as a transcript the summariser can read: whole texts,
/// tool calls as one line each, tool results cut short.
pub fn render(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            Message::System(_) => {}
            Message::User(s) => out.push_str(&format!("### User\n{}\n\n", clip(s, TEXT_CHARS))),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if !content.trim().is_empty() {
                    out.push_str(&format!("### Worker\n{}\n\n", clip(content, TEXT_CHARS)));
                }
                for c in tool_calls {
                    out.push_str(&format!(
                        "### Tool call\n{} {}\n\n",
                        c.name,
                        clip(&c.arguments.to_string(), 300)
                    ));
                }
            }
            Message::Tool { content, .. } => {
                out.push_str(&format!(
                    "### Tool result\n{}\n\n",
                    clip(content, TOOL_RESULT_CHARS)
                ));
            }
        }
        if out.len() > RENDER_CHARS {
            out.push_str("(the rest is cut)\n");
            break;
        }
    }
    out
}

/// Four characters to a token: close enough to size a conversation.
pub fn estimate(messages: &[Message]) -> u32 {
    let chars: usize = messages
        .iter()
        .map(|m| match m {
            Message::System(s) | Message::User(s) => s.len(),
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                content.len()
                    + reasoning.as_ref().map_or(0, String::len)
                    + tool_calls
                        .iter()
                        .map(|c| c.name.len() + c.arguments.to_string().len())
                        .sum::<usize>()
            }
            Message::Tool { content, .. } => content.len(),
        })
        .sum();
    (chars / 4) as u32
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lm::{LmError, Reply, Request, Sender, StreamEvent};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    fn user(s: &str) -> Message {
        Message::User(s.into())
    }
    fn answer(s: &str) -> Message {
        Message::Assistant {
            content: s.into(),
            reasoning: None,
            tool_calls: vec![],
        }
    }

    #[test]
    fn the_last_task_is_kept_and_a_lone_task_is_old() {
        let msgs = vec![
            user("## Project\nFolder: /p\n\n## Task\nfirst"),
            answer("done one"),
            user(&format!("{}\n\n- fix", prompts::WORKER_FIX)),
            answer("fixed"),
            user("second"),
            Message::Tool {
                call_id: "c".into(),
                content: "x".into(),
            },
            answer("done two"),
        ];
        let (old, kept) = split(&msgs);
        assert_eq!(old.len(), 4);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0], user("second"));
        let (old, kept) = split(&msgs[..2]);
        assert_eq!((old.len(), kept.len()), (2, 0));
    }

    #[test]
    fn rendering_cuts_tool_results_and_sizes_are_rough() {
        let big = "y".repeat(5_000);
        let msgs = vec![
            user("look at a.rs"),
            Message::Assistant {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![crate::lm::ToolCall {
                    id: "c".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.rs"}),
                }],
            },
            Message::Tool {
                call_id: "c".into(),
                content: big.clone(),
            },
            answer("a.rs holds main."),
        ];
        let text = render(&msgs);
        assert!(
            text.contains("### Tool call\nread_file {\"path\":\"a.rs\"}"),
            "{text}"
        );
        assert!(text.len() < 1_500, "the big result was cut: {}", text.len());
        assert!(estimate(&msgs) > 1_250 && estimate(&msgs) < 1_300);
    }

    /// Answers once with a fixed note.
    struct Noter(Mutex<Option<String>>);

    #[async_trait]
    impl Model for Noter {
        async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
            // The summariser must see the old tasks, not the kept one.
            let shown = match &req.messages[1] {
                Message::User(s) => s.clone(),
                _ => String::new(),
            };
            assert!(
                shown.contains("first") && !shown.contains("second"),
                "{shown}"
            );
            let note = self.0.lock().unwrap().take().unwrap();
            let _ = tx.send(StreamEvent::Text(note.clone())).await;
            Ok(Reply {
                content: note,
                ..Default::default()
            })
        }
        fn name(&self) -> &str {
            "noter"
        }
    }

    #[tokio::test]
    async fn compaction_leaves_a_note_and_the_last_task() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let (events, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            user("first"),
            answer("done one"),
            user("second"),
            answer("done two"),
        ];
        let model = Noter(Mutex::new(Some("- first: done.".into())));
        let report = compact(
            &model,
            &ctx,
            events,
            CancellationToken::new(),
            &mut messages,
            40_000,
        )
        .await
        .unwrap();
        assert_eq!(report.tasks, 1);
        assert_eq!(report.before, 40_000);
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0], Message::System("sys".into()));
        assert!(
            matches!(&messages[1], Message::User(s) if s.starts_with("## Earlier in this session") && s.contains("- first: done."))
        );
        assert_eq!(messages[3], user("second"));
        // The note is not a task, so the earlier-tasks digest skips it.
        assert_eq!(
            crate::agent::convo::exchanges(&messages[1..]),
            [("second".to_string(), "done two".to_string())]
        );
    }
}
