//! `todo`: the worker's checklist for a task with several steps, and how the
//! user sees where it is. The list lives as long as the session, so a task
//! that stops with steps open hands them to the next ("continue").

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError};
use crate::agent::AgentEvent;
use crate::lm::ToolSpec;

/// A step longer than this is a paragraph; the list shows one line each.
const MAX_CHARS: usize = 200;
const MAX_ITEMS: usize = 20;

/// What a reviewer's ask starts with on the checklist.
pub const FIX: &str = "Fix: ";
/// What a `## Done when` line starts with on the checklist.
pub const TEST: &str = "Test: ";

/// A step weft put on the list, not the worker: the plan's tests and the
/// reviewer's asks. A new list may tick one off, but not leave it out.
fn is_kept(t: &Todo) -> bool {
    t.text.starts_with(TEST) || t.text.starts_with(FIX)
}

/// A step's words for matching: case and spacing aside.
fn words(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The open steps weft put on the list that `new` leaves out. A step
/// reworded but kept under its prefix counts as kept, one for one.
fn dropped(old: &[Todo], new: &[Todo]) -> Vec<String> {
    let have: Vec<String> = new.iter().map(|t| words(&t.text)).collect();
    let before: Vec<String> = old.iter().map(|t| words(&t.text)).collect();
    let missing: Vec<&Todo> = old
        .iter()
        .filter(|t| t.is_open() && is_kept(t) && !have.contains(&words(&t.text)))
        .collect();
    let reworded = |prefix: &str| {
        new.iter()
            .filter(|t| t.text.starts_with(prefix) && !before.contains(&words(&t.text)))
            .count()
    };
    let (mut tests, mut fixes) = (reworded(TEST), reworded(FIX));
    missing
        .into_iter()
        .filter(|t| {
            let spare = if t.text.starts_with(TEST) {
                &mut tests
            } else {
                &mut fixes
            };
            if *spare > 0 {
                *spare -= 1;
                false
            } else {
                true
            }
        })
        .map(|t| t.text.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub text: String,
    pub status: Status,
}

impl Todo {
    pub fn pending(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            status: Status::Pending,
        }
    }

    pub fn is_open(&self) -> bool {
        self.status != Status::Done
    }

    /// `☒` done, `◼` under way, `☐` not started.
    pub fn mark(&self) -> &'static str {
        match self.status {
            Status::Done => "☒",
            Status::InProgress => "◼",
            Status::Pending => "☐",
        }
    }
}

/// The session's list, shared by the task's tools and the tandem.
pub type Todos = Arc<Mutex<Vec<Todo>>>;

/// How many are done, of how many.
pub fn progress(list: &[Todo]) -> (usize, usize) {
    (list.iter().filter(|t| !t.is_open()).count(), list.len())
}

/// The list as the model reads it: one `☐ step` line each.
pub fn checklist(list: &[Todo]) -> String {
    list.iter()
        .map(|t| format!("{} {}", t.mark(), t.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace the list and tell the screen.
pub async fn set(ctx: &Ctx, list: Vec<Todo>) {
    *ctx.todos.lock().unwrap() = list.clone();
    if let Some(events) = &ctx.events {
        let _ = events.send(AgentEvent::Todos { items: list }).await;
    }
}

/// A status as the model writes it; it borrows other tools' words too.
fn status_of(s: &str) -> Option<Status> {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
        "pending" | "todo" | "open" | "not_started" => Some(Status::Pending),
        "in_progress" | "active" | "doing" | "started" => Some(Status::InProgress),
        "done" | "completed" | "complete" | "finished" => Some(Status::Done),
        _ => None,
    }
}

/// The `todos` argument: a list, or that list as JSON text when the raw route
/// could not read it as one.
fn parse(args: &Value) -> Result<Vec<Todo>, ToolError> {
    let bad = |m: String| ToolError::BadArgs(m);
    let raw = match args.get("todos") {
        Some(Value::String(s)) => serde_json::from_str(s)
            .map_err(|e| bad(format!("`todos` must be a list of {{text, status}}: {e}")))?,
        Some(v) => v.clone(),
        None => return Err(bad("`todos` is required; send [] to clear the list".into())),
    };
    let items = raw
        .as_array()
        .ok_or_else(|| bad("`todos` must be a list of {text, status}".into()))?;
    if items.len() > MAX_ITEMS {
        return Err(bad(format!(
            "{} steps; keep the list under {MAX_ITEMS}, a line each",
            items.len()
        )));
    }
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let text = item
            .get("text")
            .or_else(|| item.get("content"))
            .and_then(Value::as_str)
            .map(|t| t.trim().replace('\n', " "))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| bad(format!("step {} has no `text`", i + 1)))?;
        if text.chars().count() > MAX_CHARS {
            return Err(bad(format!(
                "step {} is {} characters; one line, under {MAX_CHARS}",
                i + 1,
                text.chars().count()
            )));
        }
        let given = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let status = status_of(given).ok_or_else(|| {
            bad(format!(
                "step {}: status `{given}` is not pending, in_progress or done",
                i + 1
            ))
        })?;
        out.push(Todo { text, status });
    }
    Ok(out)
}

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".into(),
            description: "Your checklist for this task; the user watches it to see where you are. \
                          Send the whole list every time, each step with its status: pending, \
                          in_progress or done. Keep one step in_progress while you work on it and \
                          mark it done as soon as its check passes. Use it for work of three or \
                          more steps, not for a quick change or a question. [] clears it."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Every step, in order",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "description": "The step, one short line" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "done"] }
                            },
                            "required": ["text", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let list = parse(args)?;
        // The plan's tests and the reviewer's asks stay until they are done:
        // a fresh list that leaves them out would let them go unwritten.
        let gone = dropped(&ctx.todos.lock().unwrap(), &list);
        if !gone.is_empty() {
            return Err(ToolError::BadArgs(format!(
                "the list leaves out {} that weft added and that are not done:\n{}\n\
                 Send the list again with each of them, word for word: mark one done \
                 once it is written and passes, or keep it pending and say in your \
                 answer why it cannot be done.",
                if gone.len() == 1 { "a step" } else { "steps" },
                gone.iter()
                    .map(|t| format!("☐ {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
        }
        let (done, total) = progress(&list);
        let reply = if total == 0 {
            "The list is clear.".to_string()
        } else if done == total {
            format!("All {total} done.\n{}", checklist(&list))
        } else {
            format!(
                "{done} of {total} done.\n{}\nCarry on with the step in progress.",
                checklist(&list)
            )
        };
        set(ctx, list).await;
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_list_is_replaced_and_the_screen_hears_of_it() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf()).for_task(Some(tx), Default::default());
        let out = TodoWrite
            .run(
                &json!({"todos": [
                    {"text": "Add the op", "status": "completed"},
                    {"text": "Teach the coach", "status": "in-progress"},
                    {"text": "Run the tests"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            out.starts_with("1 of 3 done.\n☒ Add the op\n◼ Teach the coach\n☐ Run the tests"),
            "{out}"
        );
        assert_eq!(ctx.todos.lock().unwrap()[1].status, Status::InProgress);
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::Todos { items }) if items.len() == 3));
    }

    #[tokio::test]
    async fn the_list_may_arrive_as_text_and_bad_steps_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let text = json!({"todos": "[{\"text\":\"a\",\"status\":\"done\"}]"});
        assert_eq!(
            TodoWrite.run(&text, &ctx).await.unwrap(),
            "All 1 done.\n☒ a"
        );
        let bad = json!({"todos": [{"text": "a", "status": "maybe"}]});
        assert!(matches!(
            TodoWrite.run(&bad, &ctx).await,
            Err(ToolError::BadArgs(_))
        ));
        assert_eq!(
            ctx.todos.lock().unwrap().len(),
            1,
            "a refused list leaves the old one"
        );
    }

    #[tokio::test]
    async fn the_plans_tests_and_the_reviewers_asks_cannot_be_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        *ctx.todos.lock().unwrap() = vec![
            Todo::pending("`a.rs`: sort the tiles"),
            Todo::pending("Test: alerting tiles come first"),
            Todo::pending("Fix: the empty group"),
            Todo {
                text: "Test: done already".into(),
                status: Status::Done,
            },
        ];

        // The worker's own list, without weft's open steps: refused, naming them.
        let own = json!({"todos": [{"text": "Sort the tiles", "status": "in_progress"}]});
        let Err(ToolError::BadArgs(why)) = TodoWrite.run(&own, &ctx).await else {
            panic!("a list without the tests was taken");
        };
        assert!(why.contains("☐ Test: alerting tiles come first"), "{why}");
        assert!(why.contains("☐ Fix: the empty group"), "{why}");
        assert!(!why.contains("done already"), "{why}");
        assert_eq!(ctx.todos.lock().unwrap().len(), 4);

        // Kept (in other case or spacing), reworded under the prefix, or
        // ticked off: taken. A done step may go.
        let kept = json!({"todos": [
            {"text": "Sort the tiles", "status": "done"},
            {"text": "Test:  Alerting tiles come FIRST", "status": "in_progress"},
            {"text": "Fix: the empty group keeps its place", "status": "done"}
        ]});
        assert!(TodoWrite.run(&kept, &ctx).await.is_ok());
        assert_eq!(ctx.todos.lock().unwrap().len(), 3);

        // Once done, they may leave the list.
        let next = json!({"todos": [
            {"text": "Test: Alerting tiles come first", "status": "done"}
        ]});
        assert!(TodoWrite.run(&next, &ctx).await.is_ok());
        assert!(TodoWrite.run(&json!({"todos": []}), &ctx).await.is_ok());
    }
}
