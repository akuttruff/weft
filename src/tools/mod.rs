//! The worker's hands: read, list, search, edit, write, run. Each tool is a
//! struct behind the [`Tool`] trait; the [`Registry`] offers their specs to the
//! model and runs the calls that come back.
//!
//! Every path a tool touches is resolved against the project root and refused
//! if it points outside. Output is capped so one call cannot flood the context.

pub mod bash;
pub mod files;
pub mod image;
pub mod memory;
pub mod search;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::{Approver, Events};
use crate::checkpoint::Checkpoint;
use crate::hooks::{self, Hooks, On};
use crate::lm::{ToolCall, ToolSpec};

/// What the tools know about where they are, and about the task they run in.
#[derive(Clone)]
pub struct Ctx {
    /// The project root; nothing outside it may be read or written.
    pub root: PathBuf,
    /// Cancelled when the user stops the task; a long tool should stop too.
    pub cancel: CancellationToken,
    /// Where a tool that runs a model of its own (the scout) reports its turns.
    pub events: Option<Events>,
    /// Where files are kept as they were before this task changes them, for `/undo`.
    pub checkpoint: Option<Arc<Checkpoint>>,
    /// Who answers approval questions, for a tool that runs a worker of its own (the delegate).
    pub approver: Option<Arc<dyn Approver>>,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx").field("root", &self.root).finish()
    }
}

impl Ctx {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cancel: CancellationToken::new(),
            events: None,
            checkpoint: None,
            approver: None,
        }
    }

    /// The same folder, tied to one task's stop signal, event stream,
    /// checkpoint and approver.
    pub fn for_task(
        &self,
        events: Option<Events>,
        cancel: CancellationToken,
        checkpoint: Option<Arc<Checkpoint>>,
        approver: Option<Arc<dyn Approver>>,
    ) -> Self {
        Self {
            root: self.root.clone(),
            cancel,
            events,
            checkpoint,
            approver,
        }
    }
}

/// Keep `path` as it is before a tool changes it, when a checkpoint is being kept.
pub fn keep_before_change(ctx: &Ctx, path: &Path) -> Result<(), ToolError> {
    if let Some(cp) = &ctx.checkpoint
        && let Ok(rel) = path.strip_prefix(&ctx.root)
    {
        cp.before_write(&ctx.root, &rel.to_string_lossy())?;
    }
    Ok(())
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

/// Calls that run without asking: `[approve]` in the config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Rules {
    /// Tools that never ask, by name.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell commands that never ask, by leading words: `cargo test` lets
    /// `cargo test --lib` through, not `cargo testx`. Every part of a chain
    /// (`&&`, `;`, `|`, a newline) must match, and nothing with a `$(…)` or a
    /// backtick in it does.
    #[serde(default)]
    pub commands: Vec<String>,
}

impl Rules {
    pub fn allows(&self, name: &str, args: &Value) -> bool {
        if self.tools.iter().any(|t| t == name) {
            return true;
        }
        match (name, args.get("command").and_then(Value::as_str)) {
            ("bash", Some(cmd)) => self.allows_command(cmd),
            _ => false,
        }
    }

    pub fn allows_command(&self, cmd: &str) -> bool {
        if self.commands.is_empty() || cmd.contains("$(") || cmd.contains('`') {
            return false;
        }
        cmd.split(['\n', ';', '|'])
            .flat_map(|part| part.split("&&"))
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .all(|part| self.commands.iter().any(|c| leads_with(part, c)))
    }
}

/// `cmd` is `words`, or `words` followed by a space.
fn leads_with(cmd: &str, words: &str) -> bool {
    let words = words.trim();
    !words.is_empty()
        && (cmd == words
            || cmd
                .strip_prefix(words)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace)))
}

/// All the tools, by name.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
    rules: Rules,
    hooks: Hooks,
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
            rules: Rules::default(),
            hooks: Hooks::default(),
        }
    }

    /// The same tools, with these calls allowed without asking.
    pub fn with_rules(mut self, rules: Rules) -> Self {
        self.rules = rules;
        self
    }

    /// The same tools, with these hooks around edits and shell commands.
    pub fn with_hooks(mut self, hooks: Vec<crate::hooks::Hook>) -> Self {
        self.hooks = Hooks(hooks);
        self
    }

    /// Looking only: what the planner gets.
    pub fn read_only() -> Self {
        Self {
            tools: vec![
                Box::new(files::ReadFile),
                Box::new(files::ListFiles),
                Box::new(search::Search),
            ],
            rules: Rules::default(),
            hooks: Hooks::default(),
        }
    }

    /// No tools: a single question and answer.
    pub fn empty() -> Self {
        Self {
            tools: vec![],
            rules: Rules::default(),
            hooks: Hooks::default(),
        }
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

    /// What approving this call would allow, if it needs approval. A call the
    /// config's rules allow needs none.
    pub fn approval(&self, call: &ToolCall) -> Option<String> {
        if self.rules.allows(&call.name, &call.arguments) {
            return None;
        }
        self.get(&call.name)?.approval(&call.arguments)
    }

    pub async fn run(&self, call: &ToolCall, ctx: &Ctx) -> Result<String, ToolError> {
        let tool = self
            .get(&call.name)
            .ok_or_else(|| ToolError::Unknown(call.name.clone()))?;
        let arg = |k: &str| call.arguments.get(k).and_then(Value::as_str).unwrap_or("");
        // A before_bash hook that fails stops the command; what it printed is the reason.
        if call.name == "bash" && self.hooks.has(On::BeforeBash) {
            let outcomes = self
                .hooks
                .run(
                    On::BeforeBash,
                    &ctx.root,
                    &[("WEFT_COMMAND", arg("command"))],
                )
                .await;
            if let Some(no) = outcomes.iter().find(|o| !o.ok) {
                let why = if no.output.is_empty() {
                    &no.run
                } else {
                    &no.output
                };
                return Err(ToolError::Refused(format!(
                    "a hook stopped this command: {why}"
                )));
            }
        }
        let mut out = tool.run(&call.arguments, ctx).await?;
        // After an edit, the hooks' output (a formatter's say) goes to the model too.
        if matches!(call.name.as_str(), "edit_file" | "write_file") && self.hooks.has(On::AfterEdit)
        {
            let outcomes = self
                .hooks
                .run(On::AfterEdit, &ctx.root, &[("WEFT_PATH", arg("path"))])
                .await;
            let said = hooks::describe(&outcomes);
            if !said.is_empty() {
                out.push('\n');
                out.push_str(&said);
            }
        }
        Ok(out)
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

    #[tokio::test]
    async fn hooks_guard_shell_commands_and_speak_after_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin().with_hooks(vec![
            crate::hooks::Hook {
                on: On::BeforeBash,
                run: "case \"$WEFT_COMMAND\" in *rm*) echo no deleting; exit 1;; esac".into(),
            },
            crate::hooks::Hook {
                on: On::AfterEdit,
                run: "echo tidied $WEFT_PATH".into(),
            },
        ]);
        let call = |name: &str, args: Value| ToolCall {
            id: "c".into(),
            name: name.into(),
            arguments: args,
        };
        let refused = registry
            .run(
                &call("bash", serde_json::json!({"command": "rm a.txt"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(&refused, Err(ToolError::Refused(m)) if m.contains("no deleting")),
            "{refused:?}"
        );
        let ok = registry
            .run(
                &call("bash", serde_json::json!({"command": "echo fine"})),
                &ctx,
            )
            .await
            .unwrap();
        assert!(ok.contains("fine"));
        let edited = registry
            .run(
                &call(
                    "edit_file",
                    serde_json::json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"}),
                ),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            edited,
            "edited a.txt:1 (+1 −1)\nhook `echo tidied $WEFT_PATH`: tidied a.txt"
        );
    }

    #[test]
    fn rules_match_leading_words_in_every_part_of_a_chain() {
        let rules = Rules {
            tools: vec!["edit_file".into()],
            commands: vec![
                "cargo test".into(),
                "git status".into(),
                "ls".into(),
                "head".into(),
            ],
        };
        let bash = |c: &str| serde_json::json!({"command": c});
        assert!(rules.allows("edit_file", &serde_json::json!({})));
        assert!(!rules.allows("write_file", &serde_json::json!({})));
        assert!(rules.allows("bash", &bash("cargo test --lib")));
        assert!(rules.allows("bash", &bash("git status && ls -la | head")));
        assert!(!rules.allows("bash", &bash("cargo testx")));
        assert!(!rules.allows("bash", &bash("ls; rm -rf x")));
        assert!(!rules.allows("bash", &bash("ls $(rm x)")));
        assert!(!Rules::default().allows("bash", &bash("ls")));
    }

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
