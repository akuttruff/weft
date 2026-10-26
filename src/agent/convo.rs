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

/// A task that stopped early (esc, the window full, the model gone) leaves
/// its steps in the conversation rather than losing them: a trailing call
/// without all its results is cut, and a closing line says what happened.
/// Returns false when nothing of the task was left to keep.
pub fn close_interrupted(messages: &mut Vec<Message>, from: usize, reason: &str) -> bool {
    // The last turn with tool calls must have every result after it.
    if let Some(i) = messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()))
        && i >= from
        && let Message::Assistant { tool_calls, .. } = &messages[i]
    {
        let results = messages[i + 1..]
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .count();
        if results < tool_calls.len() {
            messages.truncate(i);
        }
    }
    // Only the task's own message survived: nothing was done, drop it too.
    if messages.len() <= from + 1 {
        messages.truncate(from);
        return false;
    }
    messages.push(Message::Assistant {
        content: format!("(stopped here: {reason})"),
        reasoning: None,
        tool_calls: vec![],
    });
    true
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
    fn an_interrupted_task_keeps_its_finished_steps() {
        let call = |id: &str| crate::lm::ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        };
        let asst = |calls: Vec<crate::lm::ToolCall>| Message::Assistant {
            content: String::new(),
            reasoning: None,
            tool_calls: calls,
        };
        let tool = |id: &str| Message::Tool {
            call_id: id.into(),
            content: "1\tfn".into(),
        };
        // Two whole rounds, then a call that never got its result.
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("do it".into()),
            asst(vec![call("a")]),
            tool("a"),
            asst(vec![call("b"), call("c")]),
            tool("b"),
        ];
        assert!(close_interrupted(&mut messages, 1, "the user stopped it"));
        assert_eq!(
            messages.len(),
            5,
            "the half-answered turn went: {messages:?}"
        );
        assert!(
            matches!(&messages[4], Message::Assistant { content, .. } if content.contains("stopped here"))
        );
        assert_eq!(
            exchanges(&messages[1..]),
            [(
                "do it".to_string(),
                "(stopped here: the user stopped it)".to_string()
            )]
        );
        // Nothing beyond the task itself: nothing is kept.
        let mut bare = vec![Message::System("sys".into()), Message::User("do it".into())];
        assert!(!close_interrupted(&mut bare, 1, "x"));
        assert_eq!(bare.len(), 1);
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
