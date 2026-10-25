//! The worker's hands: read, list, search, edit, write, run. Each tool is a
//! struct behind the [`Tool`] trait; the [`Registry`] offers their specs to the
//! model and runs the calls that come back.
//!
//! Every path a tool touches is resolved against the project root and refused
//! if it points outside. Output is capped so one call cannot flood the context.

pub mod bash;
pub mod files;
pub mod search;

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::Events;
use crate::lm::{ToolCall, ToolSpec};

/// What the tools know about where they are, and about the task they run in.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// The project root; nothing outside it may be read or written.
    pub root: PathBuf,
    /// Cancelled when the user stops the task; a long tool should stop too.
    pub cancel: CancellationToken,
    /// Where a tool that runs a model of its own (the scout) reports its turns.
    pub events: Option<Events>,
}

impl Ctx {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cancel: CancellationToken::new(),
            events: None,
        }
    }

    /// The same folder, tied to one task's stop signal and event stream.
    pub fn for_task(&self, events: Option<Events>, cancel: CancellationToken) -> Self {
        Self {
            root: self.root.clone(),
            cancel,
            events,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("no tool called {0}")]
    Unknown(String),
    #[error("{0}")]
    BadArgs(String),
    #[error("{0} is outside the project")]
    Outside(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One tool the model can call.
// Rust: a trait is a set of methods a type promises to have. `async_trait`
// lets the methods be `async` while the trait is still usable as `dyn Tool`,
// which plain async fns in traits cannot yet be.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The name, description and argument schema shown to the model.
    fn spec(&self) -> ToolSpec;

    /// If this call must be approved first, a short line saying what it would do.
    fn approval(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Run it. The string comes back to the model as the tool result.
    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError>;
}

/// All the tools, by name.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Registry {
    /// The built-in set.
    pub fn builtin() -> Self {
        Self {
            tools: vec![
                Box::new(files::ReadFile),
                Box::new(files::ListFiles),
                Box::new(search::Search),
                Box::new(files::EditFile),
                Box::new(files::WriteFile),
                Box::new(bash::Bash),
            ],
        }
    }

    /// Looking only: what the planner gets.
    pub fn read_only() -> Self {
        Self {
            tools: vec![
                Box::new(files::ReadFile),
                Box::new(files::ListFiles),
                Box::new(search::Search),
            ],
        }
    }

    /// No tools: a single question and answer.
    pub fn empty() -> Self {
        Self { tools: vec![] }
    }

    /// More tools (an MCP server's), after the built-in ones.
    pub fn extend(&mut self, tools: Vec<Box<dyn Tool>>) {
        self.tools.extend(tools);
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        // Rust: `Box<dyn Tool>` derefs to `&dyn Tool` with `as_ref`; the box
        // stays owned by the Vec, we only borrow what is in it.
        self.tools
            .iter()
            .find(|t| t.spec().name == name)
            .map(|t| t.as_ref())
    }

    /// What approving this call would allow, if it needs approval.
    pub fn approval(&self, call: &ToolCall) -> Option<String> {
        self.get(&call.name)?.approval(&call.arguments)
    }

    pub async fn run(&self, call: &ToolCall, ctx: &Ctx) -> Result<String, ToolError> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| ToolError::Unknown(call.name.clone()))?;
        tool.run(&call.arguments, ctx).await
    }
}

// ---- Shared helpers ---------------------------------------------------------

/// A required string argument.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs(format!("`{key}` is required")))
}

/// An optional whole-number argument.
pub fn arg_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .map(Some)
            .ok_or_else(|| ToolError::BadArgs(format!("`{key}` must be a whole number"))),
    }
}

/// `path` under `root`, normalised, or an error if it would escape. The file
/// need not exist yet (so `write_file` can create it).
pub fn resolve(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let given = Path::new(path.trim());
    let joined = if given.is_absolute() {
        given.to_path_buf()
    } else {
        root.join(given)
    };
    // Fold `.` and `..` without touching the disk.
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(ToolError::Outside(path.to_string()));
                }
            }
            other => out.push(other),
        }
    }
    let root_real = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // Follow symlinks in whatever part of the path already exists.
    let real = nearest_real(&out);
    if real.starts_with(&root_real) {
        Ok(out)
    } else {
        Err(ToolError::Outside(path.to_string()))
    }
}

/// The canonical form of the longest existing prefix of `path`, plus the rest.
fn nearest_real(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Ok(real) = cur.canonicalize() {
            let mut out = real;
            for part in missing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The path as shown to the model: relative to the root where possible.
pub fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// Keep at most `max_lines` lines and `max_bytes` bytes, saying what was dropped.
pub fn cap(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    let mut shown = 0;
    for line in &lines {
        if shown >= max_lines || out.len() + line.len() + 1 > max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
        shown += 1;
    }
    if shown < lines.len() {
        out.push_str(&format!(
            "[... {} more lines not shown]\n",
            lines.len() - shown
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_keeps_paths_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(
            resolve(root, "src/main.rs").unwrap(),
            root.join("src/main.rs")
        );
        assert_eq!(resolve(root, "./src/../src/x").unwrap(), root.join("src/x"));
        assert!(matches!(
            resolve(root, "../etc/passwd"),
            Err(ToolError::Outside(_))
        ));
        assert!(matches!(
            resolve(root, "/etc/passwd"),
            Err(ToolError::Outside(_))
        ));
        assert_eq!(
            resolve(root, &root.join("a").display().to_string()).unwrap(),
            root.join("a")
        );
    }

    #[test]
    fn resolve_sees_through_a_symlink_out_of_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(matches!(
            resolve(dir.path(), "link/secret"),
            Err(ToolError::Outside(_))
        ));
    }

    #[test]
    fn cap_says_what_it_dropped() {
        let text = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = cap(&text, 3, 10_000);
        assert_eq!(
            out,
            "line 1\nline 2\nline 3\n[... 7 more lines not shown]\n"
        );
        assert_eq!(cap("a\nb", 10, 10_000), "a\nb\n");
    }

    #[test]
    fn registry_knows_its_tools() {
        let reg = Registry::builtin();
        let names: Vec<_> = reg.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "list_files",
                "search",
                "edit_file",
                "write_file",
                "bash"
            ]
        );
        assert!(reg.get("nope").is_none());
    }
}
