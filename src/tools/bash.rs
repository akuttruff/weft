//! Running a shell command in the project, with a timeout and capped output.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
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
        let timeout = arg_u64(args, "timeout_s")?
            .unwrap_or(DEFAULT_TIMEOUT_S)
            .min(MAX_TIMEOUT_S);

        let mut child = tokio::process::Command::new("/bin/zsh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.root)
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
        Ok(shown)
    }
}

fn forbidden(command: &str) -> Option<&'static str> {
    FORBIDDEN.iter().copied().find(|bad| command.contains(bad))
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
