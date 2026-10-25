//! An edit shown as code: the lines a tool removed and added, numbered as
//! they sit in the file. Built from the call's arguments once the tool has
//! run, so the model's own context never carries a diff it did not ask for.

use serde_json::Value;
use similar::{ChangeTag, TextDiff};

/// Rows shown before the rest is folded into `… n more lines`.
pub const MAX_ROWS: usize = 40;

/// One line of the shown diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Where the line sits: in the old file for a removed line, in the new
    /// file otherwise.
    pub line: usize,
    pub kind: Kind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Same,
    Removed,
    Added,
}

/// The rows to show for a finished tool call, and how many were left out.
/// `None` for tools that do not change files, or when the call failed.
pub fn rows(tool: &str, args: &Value, output: &str) -> Option<(Vec<Row>, usize)> {
    let s = |k: &str| args.get(k).and_then(Value::as_str);
    let all = match tool {
        "edit_file" => edit(s("old_string")?, s("new_string")?, start_line(output)),
        "write_file" => s("content")?
            .lines()
            .enumerate()
            .map(|(i, l)| Row {
                line: i + 1,
                kind: Kind::Added,
                text: l.to_string(),
            })
            .collect(),
        _ => return None,
    };
    let hidden = all.len().saturating_sub(MAX_ROWS);
    let mut shown = all;
    shown.truncate(MAX_ROWS);
    Some((shown, hidden))
}

/// `edited src/a.rs:12 (+3 −1)` → 12. Without one, the numbers start at 1.
fn start_line(output: &str) -> usize {
    let loc = output.strip_prefix("edited ").unwrap_or(output);
    let loc = loc.rsplit_once(" (").map_or(loc, |(l, _)| l);
    loc.rsplit_once(':')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(1)
}

/// A line diff of the replaced passage against its replacement.
// Rust: `similar` gives each change its index in the old and in the new text
// as `Option<usize>`; `unwrap_or(0)` never fires for the side a change is on.
fn edit(old: &str, new: &str, start: usize) -> Vec<Row> {
    let diff = TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .map(|change| {
            let (kind, at) = match change.tag() {
                ChangeTag::Delete => (Kind::Removed, change.old_index()),
                ChangeTag::Insert => (Kind::Added, change.new_index()),
                ChangeTag::Equal => (Kind::Same, change.new_index()),
            };
            Row {
                line: start + at.unwrap_or(0),
                kind,
                text: change.value().trim_end_matches(['\n', '\r']).to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_an_edit_from_where_it_landed() {
        let args = json!({
            "path": "a.rs",
            "old_string": "fn b() {}\nfn c() {}",
            "new_string": "fn b() {\n    1\n}\nfn c() {}"
        });
        let (rows, hidden) = rows("edit_file", &args, "edited a.rs:2 (+3 −1)").unwrap();
        assert_eq!(hidden, 0);
        let shown: Vec<(usize, Kind, &str)> = rows
            .iter()
            .map(|r| (r.line, r.kind, r.text.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (2, Kind::Removed, "fn b() {}"),
                (2, Kind::Added, "fn b() {"),
                (3, Kind::Added, "    1"),
                (4, Kind::Added, "}"),
                (5, Kind::Same, "fn c() {}"),
            ]
        );
    }

    #[test]
    fn a_new_file_is_all_additions_and_long_ones_fold() {
        let content = (1..=50)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (rows, hidden) = rows(
            "write_file",
            &json!({"path": "x", "content": content}),
            "created x",
        )
        .unwrap();
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(hidden, 10);
        assert_eq!(
            rows[0],
            Row {
                line: 1,
                kind: Kind::Added,
                text: "l1".into()
            }
        );
        assert!(rows.iter().all(|r| r.kind == Kind::Added));
    }

    #[test]
    fn other_tools_show_nothing() {
        assert!(rows("bash", &json!({"command": "ls"}), "a\nb").is_none());
        assert_eq!(start_line("edited a.rs (+1 −1)"), 1);
        assert_eq!(start_line("edited src/a b.rs:7 (+1 −1)"), 7);
    }
}
