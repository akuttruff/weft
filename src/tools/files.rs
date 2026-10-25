//! Reading, listing, editing and creating files.

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError, arg_str, arg_u64, cap, display, resolve};
use crate::lm::ToolSpec;

/// Lines shown by one `read_file` call unless asked for fewer.
const READ_LINES: usize = 200;
const READ_BYTES: usize = 24_000;
const LIST_MAX: usize = 300;

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: format!(
                "Read a file with line numbers. Returns up to {READ_LINES} lines from `start`; \
                 read a long file in slices rather than all at once."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path relative to the project root" },
                    "start": { "type": "integer", "description": "First line to show, 1-based (default 1)" },
                    "end": { "type": "integer", "description": "Last line to show (default start + 199)" }
                },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let path = resolve(&ctx.root, arg_str(args, "path")?)?;
        let start = arg_u64(args, "start")?.unwrap_or(1).max(1) as usize;
        let end = arg_u64(args, "end")?
            .map(|e| e as usize)
            .unwrap_or(start + READ_LINES - 1);
        if end < start {
            return Err(ToolError::BadArgs("`end` is before `start`".into()));
        }
        let text = std::fs::read(&path)?;
        if text.contains(&0) {
            return Err(ToolError::Failed(format!(
                "{} is a binary file",
                display(&ctx.root, &path)
            )));
        }
        let text = String::from_utf8_lossy(&text);
        let total = text.lines().count();
        if total == 0 {
            return Ok("(empty file)".into());
        }
        if start > total {
            return Err(ToolError::BadArgs(format!("the file has {total} lines")));
        }
        let end = end.min(total).min(start + READ_LINES - 1);
        let width = end.to_string().len();
        let mut out = String::new();
        for (i, line) in text
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
        {
            out.push_str(&format!("{:>width$}\t{line}\n", i + 1));
        }
        let mut out = cap(&out, READ_LINES, READ_BYTES);
        if end < total {
            out.push_str(&format!("[lines {}-{total} follow]\n", end + 1));
        }
        Ok(out)
    }
}

pub struct ListFiles;

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_files".into(),
            description: "List files under a folder, honouring .gitignore. Folders end in `/`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Folder to list (default: the project root)" },
                    "depth": { "type": "integer", "description": "How many levels down (default 2)" }
                }
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let dir = resolve(
            &ctx.root,
            args.get("path").and_then(Value::as_str).unwrap_or("."),
        )?;
        let depth = arg_u64(args, "depth")?.unwrap_or(2) as usize;
        if !dir.is_dir() {
            return Err(ToolError::Failed(format!(
                "{} is not a folder",
                display(&ctx.root, &dir)
            )));
        }
        let mut entries = Vec::new();
        let mut hidden = 0;
        for entry in WalkBuilder::new(&dir)
            .max_depth(Some(depth))
            .hidden(true)
            .build()
            .flatten()
        {
            if entry.depth() == 0 {
                continue;
            }
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let mut name = display(&ctx.root, entry.path());
            if is_dir {
                name.push('/');
            }
            if entries.len() < LIST_MAX {
                entries.push(name);
            } else {
                hidden += 1;
            }
        }
        if entries.is_empty() {
            return Ok("(empty folder)".into());
        }
        entries.sort();
        let mut out = entries.join("\n");
        out.push('\n');
        if hidden > 0 {
            out.push_str(&format!(
                "[... {hidden} more not shown; list a subfolder]\n"
            ));
        }
        Ok(out)
    }
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create a new file with this content. Refuses to overwrite: change an \
                          existing file with edit_file."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn approval(&self, args: &Value) -> Option<String> {
        Some(format!(
            "create {}",
            args.get("path").and_then(Value::as_str).unwrap_or("?")
        ))
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let path = resolve(&ctx.root, arg_str(args, "path")?)?;
        let content = arg_str(args, "content")?;
        if path.exists() {
            return Err(ToolError::Refused(format!(
                "{} already exists; use edit_file to change it",
                display(&ctx.root, &path)
            )));
        }
        super::keep_before_change(ctx, &path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        Ok(format!(
            "created {} ({} lines)",
            display(&ctx.root, &path),
            content.lines().count()
        ))
    }
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace one exact passage in a file. `old_string` must appear exactly \
                          once, copied verbatim (indentation included) from a read_file result; \
                          include a line or two of context to make it unique."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    fn approval(&self, args: &Value) -> Option<String> {
        Some(format!(
            "edit {}",
            args.get("path").and_then(Value::as_str).unwrap_or("?")
        ))
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let path = resolve(&ctx.root, arg_str(args, "path")?)?;
        let old = arg_str(args, "old_string")?;
        let new = arg_str(args, "new_string")?;
        if old.is_empty() {
            return Err(ToolError::BadArgs("`old_string` is empty".into()));
        }
        let shown = display(&ctx.root, &path);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Failed(format!("{shown}: {e}")))?;
        match text.matches(old).count() {
            0 => Err(ToolError::Failed(format!(
                "old_string was not found in {shown}. Read the file again and copy the passage exactly."
            ))),
            1 => {
                // The line the passage starts on, so the result reads `path:line`
                // and the screen can number the diff it shows.
                let at = text.find(old).unwrap_or(0);
                let line = text[..at].matches('\n').count() + 1;
                let updated = text.replacen(old, new, 1);
                super::keep_before_change(ctx, &path)?;
                std::fs::write(&path, &updated)?;
                let (added, removed) = line_delta(old, new);
                Ok(format!("edited {shown}:{line} (+{added} −{removed})"))
            }
            n => Err(ToolError::Failed(format!(
                "old_string appears {n} times in {shown}; include more surrounding lines so it is unique."
            ))),
        }
    }
}

/// Lines added and removed, as a rough +/− for the summary line.
fn line_delta(old: &str, new: &str) -> (usize, usize) {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
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
    async fn reads_a_slice_with_line_numbers() {
        let (_d, ctx) = ctx();
        let text = (1..=300)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(ctx.root.join("a.txt"), text).unwrap();
        let out = ReadFile
            .run(&json!({"path": "a.txt", "start": 2, "end": 3}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "2\tl2\n3\tl3\n[lines 4-300 follow]\n");
        let out = ReadFile.run(&json!({"path": "a.txt"}), &ctx).await.unwrap();
        assert!(out.ends_with("200\tl200\n[lines 201-300 follow]\n"));
        assert!(out.starts_with("  1\tl1\n"));
    }

    #[tokio::test]
    async fn refuses_binary_and_missing_files() {
        let (_d, ctx) = ctx();
        std::fs::write(ctx.root.join("b.bin"), b"ab\0cd").unwrap();
        assert!(matches!(
            ReadFile.run(&json!({"path": "b.bin"}), &ctx).await,
            Err(ToolError::Failed(_))
        ));
        assert!(matches!(
            ReadFile.run(&json!({"path": "nope"}), &ctx).await,
            Err(ToolError::Io(_))
        ));
    }

    #[tokio::test]
    async fn lists_files_honouring_gitignore() {
        let (_d, ctx) = ctx();
        std::fs::create_dir_all(ctx.root.join("src")).unwrap();
        std::fs::create_dir_all(ctx.root.join("target/deep")).unwrap();
        std::fs::write(ctx.root.join(".gitignore"), "target\n").unwrap();
        std::fs::write(ctx.root.join("src/main.rs"), "").unwrap();
        std::fs::write(ctx.root.join("target/deep/x"), "").unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(&ctx.root)
            .output()
            .unwrap();
        let out = ListFiles.run(&json!({}), &ctx).await.unwrap();
        assert_eq!(out, "src/\nsrc/main.rs\n");
    }

    #[tokio::test]
    async fn edits_exactly_one_match() {
        let (_d, ctx) = ctx();
        std::fs::write(ctx.root.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        let out = EditFile
            .run(&json!({"path": "a.rs", "old_string": "fn b() {}", "new_string": "fn b() {\n    1\n}"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "edited a.rs:2 (+3 −1)");
        assert_eq!(
            std::fs::read_to_string(ctx.root.join("a.rs")).unwrap(),
            "fn a() {}\nfn b() {\n    1\n}\nfn c() {}\n"
        );

        let err = EditFile
            .run(
                &json!({"path": "a.rs", "old_string": "fn", "new_string": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("appears 3 times"));
        let err = EditFile
            .run(
                &json!({"path": "a.rs", "old_string": "zzz", "new_string": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn write_creates_but_never_overwrites() {
        let (_d, ctx) = ctx();
        let out = WriteFile
            .run(&json!({"path": "new/f.txt", "content": "a\nb\n"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "created new/f.txt (2 lines)");
        assert!(matches!(
            WriteFile
                .run(&json!({"path": "new/f.txt", "content": ""}), &ctx)
                .await,
            Err(ToolError::Refused(_))
        ));
        assert_eq!(
            WriteFile.approval(&json!({"path": "x"})),
            Some("create x".into())
        );
    }
}
