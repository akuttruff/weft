//! `remember`: one fact about the project, kept in a file outside it and shown
//! at the start of every task, in this session and the next. `WEFT.md` is
//! what you tell weft; this is what weft learned.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError, arg_str};
use crate::lm::ToolSpec;

/// A fact longer than this is a paragraph, not a fact.
const MAX_CHARS: usize = 300;

pub struct Remember {
    pub file: PathBuf,
}

/// Where a folder's kept facts live: beside its sessions.
pub fn file_for(root: &Path) -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    Some(
        home.join(".local/share/weft/memory")
            .join(format!("{}.md", crate::transcript::slug(root))),
    )
}

#[async_trait]
impl Tool for Remember {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "Keep one fact about this project for later tasks and sessions: how its \
                          tests or build run, a convention, a trap you hit, where something \
                          unobvious lives. One short standalone sentence. It is shown at the start \
                          of every task. Not for the current task's progress or anything already \
                          in the project notes."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact": { "type": "string", "description": "The fact, as one sentence" }
                },
                "required": ["fact"]
            }),
        }
    }

    async fn run(&self, args: &Value, _ctx: &Ctx) -> Result<String, ToolError> {
        let fact = arg_str(args, "fact")?.trim().replace('\n', " ");
        if fact.is_empty() {
            return Err(ToolError::BadArgs("the fact is empty".into()));
        }
        if fact.chars().count() > MAX_CHARS {
            return Err(ToolError::BadArgs(format!(
                "that is {} characters; a fact is one sentence, under {MAX_CHARS}",
                fact.chars().count()
            )));
        }
        let line = format!("- {fact}");
        let existing = std::fs::read_to_string(&self.file).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == line) {
            return Ok(format!("already kept: {fact}"));
        }
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&line);
        text.push('\n');
        std::fs::write(&self.file, text)?;
        Ok(format!("kept: {fact}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keeps_each_fact_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mem/p.md");
        let tool = Remember { file: file.clone() };
        let ctx = Ctx::new(dir.path().to_path_buf());
        let out = tool
            .run(&json!({"fact": "Tests run with `npx vitest run`."}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "kept: Tests run with `npx vitest run`.");
        let again = tool
            .run(
                &json!({"fact": " Tests run with `npx vitest run`.\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(again.starts_with("already kept"));
        tool.run(&json!({"fact": "No default exports."}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "- Tests run with `npx vitest run`.\n- No default exports.\n"
        );
        assert!(tool.run(&json!({"fact": "  "}), &ctx).await.is_err());
        assert!(
            tool.run(&json!({"fact": "x".repeat(400)}), &ctx)
                .await
                .is_err()
        );
    }
}
