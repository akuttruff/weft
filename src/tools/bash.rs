//! Running a shell command in the project, with a timeout and capped output.

use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::{Ctx, Tool, ToolError, arg_str, arg_u64, cap};
use crate::lm::ToolSpec;

const DEFAULT_TIMEOUT_S: u64 = 120;
const MAX_TIMEOUT_S: u64 = 600;
const OUT_LINES: usize = 200;
const OUT_BYTES: usize = 16_000;

/// Commands that are never run, whatever the approval. Each one has taken a
/// service down on this machine or would make a mess no review could catch.
const FORBIDDEN: &[&str] = &[
    "pkill",
    "killall",
    "rm -rf /",
    "git push --force",
    "git push -f",
];

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: format!(
                "Run a shell command in the project root and return its output (stdout and stderr, \
                 last {OUT_LINES} lines). Use it for tests, builds, git and type checks; use the \
                 file tools to read, search and edit. Pipe long output through tail."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_s": { "type": "integer", "description": format!("Seconds before it is killed (default {DEFAULT_TIMEOUT_S})") }
                },
                "required": ["command"]
            }),
        }
    }

    fn approval(&self, args: &Value) -> Option<String> {
        Some(format!(
            "run `{}`",
            args.get("command").and_then(Value::as_str).unwrap_or("?")
        ))
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let command = arg_str(args, "command")?.trim();
        if command.is_empty() {
            return Err(ToolError::BadArgs("`command` is empty".into()));
        }
        if let Some(bad) = forbidden(command) {
            return Err(ToolError::Refused(format!("`{bad}` is not allowed here")));
        }
        if let Some(why) = left_running(command) {
            return Err(ToolError::Refused(format!(
                "{why}. {}",
                live_hint(ctx.project.deploy.as_deref())
            )));
        }
        let timeout = arg_u64(args, "timeout_s")?
            .unwrap_or(DEFAULT_TIMEOUT_S)
            .min(MAX_TIMEOUT_S);

        Ok(run_shell(&ctx.root, command, timeout).await?.text)
    }
}

/// How a shell line went: whether it exited 0, and its output (the last
/// lines, with `[exit n]` after a failure).
pub struct Ran {
    pub ok: bool,
    pub text: String,
    /// All of it, uncut: for weft's own reading, not a model's.
    pub full: String,
}

/// Run `command` with zsh in `root`, killed after `timeout` seconds.
pub async fn run_shell(
    root: &std::path::Path,
    command: &str,
    timeout: u64,
) -> Result<Ran, ToolError> {
    let mut child = tokio::process::Command::new("/bin/zsh")
        .arg("-c")
        .arg(command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    // Rust: read both pipes at once with `join!`, else a full stderr pipe
    // could block the child while we wait on stdout.
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let read_both = async {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        (out, err)
    };

    let result = tokio::time::timeout(Duration::from_secs(timeout), async {
        let (out, err) = read_both.await;
        let status = child.wait().await;
        (out, err, status)
    })
    .await;

    let (out, err, status) = match result {
        Ok(x) => x,
        Err(_) => {
            let _ = child.kill().await;
            return Err(ToolError::Failed(format!("timed out after {timeout}s")));
        }
    };
    let status = status?;

    let mut text = String::from_utf8_lossy(&out).into_owned();
    if !err.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&err));
    }
    let mut shown = tail(&text, OUT_LINES, OUT_BYTES);
    if shown.is_empty() {
        shown.push_str("(no output)\n");
    }
    if !status.success() {
        shown.push_str(&format!("[exit {}]\n", status.code().unwrap_or(-1)));
    }
    Ok(Ran {
        ok: status.success(),
        text: shown,
        full: text,
    })
}

fn forbidden(command: &str) -> Option<&'static str> {
    FORBIDDEN.iter().copied().find(|bad| command.contains(bad))
}

/// Stopping a process by its number, or starting one that outlives the
/// command. Both have left a service down here: a killed PID was launchd's,
/// which then could not get its port back from the copy started with `&`.
fn left_running(command: &str) -> Option<&'static str> {
    static KILL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(^|[;&|(]|\bxargs(\s+-\S+)*)\s*kill\b").unwrap());
    // A lone `&`: not `&&`, `2>&1` or `&>`.
    static BACKGROUND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(^|[^&>])&($|[^&>\d])").unwrap());
    static DETACHED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(^|[;&|(]\s*)(nohup|disown|setsid)\b").unwrap());
    let bare = unquoted(command);
    if KILL.is_match(&bare) {
        Some("weft does not stop processes: the one on a port may be a service's, and stays down")
    } else if BACKGROUND.is_match(&bare) || DETACHED.is_match(&bare) {
        Some(
            "weft does not leave processes running: one started here holds its port after the task",
        )
    } else {
        None
    }
}

/// What to do instead, for the refusal.
fn live_hint(deploy: Option<&str>) -> String {
    match deploy {
        Some(d) => format!("To put changes live, run `{d}`."),
        None => "To put changes live, say in your answer what the user should run.".into(),
    }
}

/// The command with quoted text blanked, so a `&` in a string is not read
/// as one in the shell.
fn unquoted(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote = None;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => out.push(' '),
            None if c == '\'' || c == '"' => quote = Some(c),
            None => out.push(c),
        }
    }
    out
}

/// The last lines, as `cap` keeps the first ones.
fn tail(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return cap(text, max_lines, max_bytes);
    }
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines || bytes + line.len() + 1 > max_bytes {
            break;
        }
        bytes += line.len() + 1;
        kept.push(*line);
    }
    kept.reverse();
    let mut out = format!(
        "[... {} earlier lines not shown]\n",
        lines.len() - kept.len()
    );
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        (dir, ctx)
    }

    #[tokio::test]
    async fn runs_in_the_root_and_merges_stderr() {
        let (_d, ctx) = ctx();
        let out = Bash
            .run(&json!({"command": "echo out; echo err >&2; pwd"}), &ctx)
            .await
            .unwrap();
        assert!(out.starts_with("out\n"), "{out}");
        assert!(out.contains("err\n"), "{out}");
        assert!(
            out.contains(ctx.root.file_name().unwrap().to_str().unwrap()),
            "{out}"
        );
    }

    #[tokio::test]
    async fn reports_exit_codes_and_timeouts() {
        let (_d, ctx) = ctx();
        let out = Bash.run(&json!({"command": "exit 3"}), &ctx).await.unwrap();
        assert_eq!(out, "(no output)\n[exit 3]\n");
        let err = Bash
            .run(&json!({"command": "sleep 5", "timeout_s": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn refuses_the_forbidden_list() {
        let (_d, ctx) = ctx();
        let err = Bash
            .run(&json!({"command": "pkill -f vite"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Refused(_)));
    }

    #[test]
    fn processes_are_not_stopped_or_left_running() {
        for bad in [
            "kill 56905 56916 && sleep 2",
            "lsof -ti:8210 | xargs kill -9",
            "lsof -ti:8210 | xargs -r kill",
            "cd app && npm run preview &",
            "npm run preview & sleep 2; curl localhost:8210",
            "nohup npm start",
        ] {
            assert!(left_running(bad).is_some(), "{bad}");
        }
        for fine in [
            "npx vitest run 2>&1 | tail -n 50",
            "npm run build &> build.log && npm test",
            "grep -n 'a & b' src/x.ts",
            "grep -rn skill src",
            "echo \"kill it\"",
        ] {
            assert!(left_running(fine).is_none(), "{fine}");
        }
    }

    #[tokio::test]
    async fn the_refusal_names_the_deploy_command() {
        let (_d, mut ctx) = ctx();
        ctx.project = std::sync::Arc::new(crate::config::Project {
            deploy: Some("npm run deploy".into()),
            ..Default::default()
        });
        let err = Bash
            .run(&json!({"command": "kill 123"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("`npm run deploy`"), "{err}");
    }

    #[test]
    fn tail_keeps_the_end() {
        let text = (1..=10)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            tail(&text, 2, 1000),
            "[... 8 earlier lines not shown]\nl9\nl10\n"
        );
        assert_eq!(tail("a\nb\n", 5, 1000), "a\nb\n");
    }
}
