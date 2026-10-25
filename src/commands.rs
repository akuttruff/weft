//! Custom slash commands: a Markdown file per command, `/name` in the menu.
//!
//! `.weft/commands/<name>.md` in the project, or `~/.config/weft/commands/`
//! for every project; a project file wins on the same name. The file is the
//! task text. Header lines before the first blank line, in `key: value` form,
//! may give a `description` for the menu and a `mode` (`plan`, `go` or
//! `auto`). `$ARGUMENTS` in the body is replaced by what follows the command;
//! without it, the arguments are appended.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::agent::tandem::Mode;

#[derive(Debug, Clone, PartialEq)]
pub struct Custom {
    pub name: String,
    pub description: String,
    pub mode: Mode,
    pub body: String,
    pub from: PathBuf,
}

/// The folders searched, project first.
pub fn dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.join(".weft/commands")];
    if let Some(home) = std::env::home_dir() {
        out.push(home.join(".config/weft/commands"));
    }
    out
}

/// Every command in the folders, sorted by name; an earlier folder wins.
pub fn load(root: &Path) -> Vec<Custom> {
    load_from(&dirs(root))
}

pub fn load_from(dirs: &[PathBuf]) -> Vec<Custom> {
    let mut found: BTreeMap<String, Custom> = BTreeMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|x| x != "md") {
                continue;
            }
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            if found.contains_key(&name) {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                found.insert(name.clone(), parse(&name, &text, &path));
            }
        }
    }
    found.into_values().collect()
}

/// A file's header lines and body.
pub fn parse(name: &str, text: &str, from: &Path) -> Custom {
    let mut description = String::new();
    let mut mode = Mode::Auto;
    let mut lines = text.lines().peekable();
    // Header: `key: value` lines up to the first blank line. Anything else
    // means there is no header and the whole file is the body.
    let mut header_lines = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            break;
        }
        match line.split_once(':') {
            Some((key, value)) if !key.trim().contains(' ') => {
                let value = value.trim();
                match key.trim() {
                    "description" => description = value.to_string(),
                    "mode" => {
                        mode = match value {
                            "plan" => Mode::Plan,
                            "go" => Mode::Go,
                            _ => Mode::Auto,
                        }
                    }
                    _ => {}
                }
                header_lines += 1;
            }
            _ => {
                header_lines = 0;
                break;
            }
        }
    }
    for _ in 0..header_lines {
        lines.next();
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    if description.is_empty() {
        description = body.lines().next().unwrap_or("").chars().take(60).collect();
    }
    Custom {
        name: name.to_string(),
        description,
        mode,
        body,
        from: from.to_path_buf(),
    }
}

/// The task text for `/name args`.
pub fn expand(body: &str, args: &str) -> String {
    let args = args.trim();
    if body.contains("$ARGUMENTS") {
        body.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{args}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_and_expands_arguments() {
        let c = parse(
            "review",
            "description: review the diff\nmode: go\n\nReview the changes to $ARGUMENTS and list problems.",
            Path::new("/x/review.md"),
        );
        assert_eq!(c.description, "review the diff");
        assert_eq!(c.mode, Mode::Go);
        assert_eq!(
            expand(&c.body, "src/a.rs"),
            "Review the changes to src/a.rs and list problems."
        );
        // No header: the first line stands in as the description.
        let c = parse(
            "tidy",
            "Tidy the imports in every file.\nKeep it small.",
            Path::new("/x/tidy.md"),
        );
        assert_eq!(c.description, "Tidy the imports in every file.");
        assert_eq!(c.mode, Mode::Auto);
        assert_eq!(
            expand(&c.body, ""),
            "Tidy the imports in every file.\nKeep it small."
        );
        assert_eq!(
            expand(&c.body, "but only src/"),
            "Tidy the imports in every file.\nKeep it small.\n\nbut only src/"
        );
    }

    #[test]
    fn project_commands_win_over_global_ones() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        let global = dir.path().join("g");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&global).unwrap();
        fs::write(project.join("test.md"), "Run the project tests.").unwrap();
        fs::write(global.join("test.md"), "Run the global tests.").unwrap();
        fs::write(
            global.join("commit.md"),
            "description: commit\n\nCommit the work.",
        )
        .unwrap();
        fs::write(global.join("notes.txt"), "not a command").unwrap();
        let all = load_from(&[project, global]);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["commit", "test"]);
        assert_eq!(all[1].body, "Run the project tests.");
    }
}
