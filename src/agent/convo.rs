//! The shape of the worker's conversation, in one place: how a task is put
//! into a user message, and how it is found again. Resume, compaction and
//! the earlier-tasks digest all read what `task_message` writes.

use super::prompts;
use crate::lm::Message;

/// The heading a task sits under when a note comes before it.
pub const TASK_HEADING: &str = "## Task\n";

/// The user message for a task: `lead` (a project note, a resume note) above
/// it, the plan below it. With a lead, the task gets its heading so it can be
/// found again.
pub fn task_message(lead: &str, task: &str, plan: Option<&str>) -> String {
    let head = if lead.is_empty() { "" } else { TASK_HEADING };
    match plan {
        Some(p) => format!("{lead}{head}{task}\n\n{}\n\n{p}", prompts::WORKER_WITH_PLAN),
        None => format!("{lead}{head}{task}"),
    }
}

/// The task in one of the worker's user messages, or `None` for a message
/// that is not a task: a fix request (part of the task before it) or a note
/// such as the project snapshot or a summary, which open with `## `.
pub fn task_of(s: &str) -> Option<String> {
    if s.starts_with(prompts::WORKER_FIX) {
        return None;
    }
    let body = match s.split_once(TASK_HEADING) {
        Some((_, after)) => after,
        None if s.starts_with("## ") => return None,
        None => s,
    };
    // The plan was appended after a blank line; the task is what came before it.
    Some(body.split("\n\n").next().unwrap_or(body).trim().to_string())
}

/// The tasks of a conversation and the answers they got, in order. A task is
/// paired with the first answer that follows it; a fix round's answer belongs
/// to the same task and is not a new pair.
pub fn exchanges(messages: &[Message]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut task: Option<String> = None;
    for m in messages {
        match m {
            Message::User(s) => {
                if let Some(t) = task_of(s) {
                    task = Some(t);
                }
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } if tool_calls.is_empty() => {
                if let Some(t) = task.take() {
                    pairs.push((t, content.trim().to_string()));
                }
            }
            _ => {}
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_message_round_trips() {
        for lead in [
            "",
            "## Project\nFolder: /p\n\n",
            "## Resumed\nlater\n\n## Project\nx\n\n",
        ] {
            for plan in [None, Some("## Plan\n1. do it")] {
                let msg = task_message(lead, "sort the tiles", plan);
                assert_eq!(task_of(&msg).as_deref(), Some("sort the tiles"), "{msg:?}");
            }
        }
        assert_eq!(task_of(&format!("{}\n\n- fix", prompts::WORKER_FIX)), None);
        assert_eq!(task_of("## Earlier in this session\nnote"), None);
    }

    #[test]
    fn exchanges_pair_tasks_with_first_answers() {
        let answer = |s: &str| Message::Assistant {
            content: s.into(),
            reasoning: None,
            tool_calls: vec![],
        };
        let messages = vec![
            Message::System("sys".into()),
            Message::User(task_message("## Project\nx\n\n", "first", Some("plan"))),
            answer("Done one."),
            Message::User(format!("{}\n\n- rename", prompts::WORKER_FIX)),
            answer("Renamed."),
            Message::User("## Undone\nnote".into()),
            answer("Noted."),
            Message::User(task_message("", "second", None)),
            answer("Done two."),
        ];
        assert_eq!(
            exchanges(&messages),
            [
                ("first".to_string(), "Done one.".to_string()),
                ("second".to_string(), "Done two.".to_string()),
            ]
        );
    }
}
