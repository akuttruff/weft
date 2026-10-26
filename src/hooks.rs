//! Hooks: shell commands of yours that run at set moments, from `[[hooks]]`
//! in the config. A formatter after every edit, a guard before every shell
//! command, a check when a task ends.
//!
//! ```toml
//! [[hooks]]
//! on = "after_edit"        # after_edit | before_bash | after_task
//! run = "cargo fmt"        # run with `sh -c` in the project root
//! ```
//!
//! The hook learns what happened through the environment: `WEFT_PATH` for
//! an edit, `WEFT_COMMAND` for a shell command, `WEFT_TOUCHED` (comma
//! separated) when a task ends. A `before_bash` hook that exits non-zero
//! stops the command, and its output is the reason the model sees.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// A hook may run this long before it is given up on.
const TIMEOUT: Duration = Duration::from_secs(60);
/// How much of a hook's output is kept.
const OUTPUT_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum On {
    AfterEdit,
    BeforeBash,
    AfterTask,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub on: On,
    pub run: String,
}

/// What one hook did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub run: String,
    pub ok: bool,
    /// stdout and stderr together, cut short.
    pub output: String,
}

#[derive(Debug, Clone, Default)]
pub struct Hooks(pub Vec<Hook>);

impl Hooks {
    pub fn has(&self, on: On) -> bool {
        self.0.iter().any(|h| h.on == on)
    }

    /// Run every hook for `on`, in order, with `env` added to the environment.
    pub async fn run(&self, on: On, root: &Path, env: &[(&str, &str)]) -> Vec<Outcome> {
        let mut out = Vec::new();
        for hook in self.0.iter().filter(|h| h.on == on) {
            out.push(run_one(&hook.run, root, env).await);
        }
        out
    }
}

async fn run_one(run: &str, root: &Path, env: &[(&str, &str)]) -> Outcome {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(run)
        .current_dir(root)
        .envs(env.iter().copied());
    cmd.stdin(std::process::Stdio::null());
    let result = tokio::time::timeout(TIMEOUT, cmd.output()).await;
    match result {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let err = String::from_utf8_lossy(&output.stderr);
            if !err.trim().is_empty() {
                if !text.trim().is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            Outcome {
                run: run.to_string(),
                ok: output.status.success(),
                output: clip(text.trim()),
            }
        }
        Ok(Err(e)) => Outcome {
            run: run.to_string(),
            ok: false,
            output: format!("could not start: {e}"),
        },
        Err(_) => Outcome {
            run: run.to_string(),
            ok: false,
            output: format!("gave up after {}s", TIMEOUT.as_secs()),
        },
    }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= OUTPUT_CHARS {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(OUTPUT_CHARS).collect::<String>())
    }
}

/// One line per outcome, for a tool result or the screen.
pub fn describe(outcomes: &[Outcome]) -> String {
    outcomes
        .iter()
        .filter(|o| !o.output.is_empty() || !o.ok)
        .map(|o| {
            let first = o.output.lines().next().unwrap_or("");
            let more = o.output.lines().count().saturating_sub(1);
            let tail = if more > 0 {
                format!(" (+{more} lines)")
            } else {
                String::new()
            };
            if o.ok {
                format!("hook `{}`: {first}{tail}", o.run)
            } else {
                format!("hook `{}` failed: {first}{tail}", o.run)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_in_order_with_the_environment_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = Hooks(vec![
            Hook {
                on: On::AfterEdit,
                run: "echo formatted $WEFT_PATH".into(),
            },
            Hook {
                on: On::BeforeBash,
                run: "echo not here; exit 1".into(),
            },
            Hook {
                on: On::AfterEdit,
                run: "pwd".into(),
            },
        ]);
        assert!(hooks.has(On::AfterEdit) && !hooks.has(On::AfterTask));
        let out = hooks
            .run(On::AfterEdit, dir.path(), &[("WEFT_PATH", "src/a.rs")])
            .await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].output, "formatted src/a.rs");
        assert!(out[0].ok);
        assert!(
            out[1]
                .output
                .ends_with(dir.path().file_name().unwrap().to_str().unwrap())
        );
        let out = hooks.run(On::BeforeBash, dir.path(), &[]).await;
        assert_eq!(
            out,
            [Outcome {
                run: "echo not here; exit 1".into(),
                ok: false,
                output: "not here".into()
            }]
        );
        assert_eq!(
            describe(&out),
            "hook `echo not here; exit 1` failed: not here"
        );
    }
}
