//! Regex search across the project, `path:line: text` per match.

use async_trait::async_trait;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError, arg_str, display, resolve};
use crate::lm::ToolSpec;

const MAX_MATCHES: usize = 100;
const MAX_LINE: usize = 240;
const MAX_FILE_BYTES: u64 = 2_000_000;

pub struct Search;

#[async_trait]
impl Tool for Search {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search".into(),
            description: format!(
                "Search file contents with a regular expression (case-insensitive). Returns up to \
                 {MAX_MATCHES} matches as path:line: text. Narrow with `path` or `glob`."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "A regular expression" },
                    "path": { "type": "string", "description": "Folder or file to search (default: the project root)" },
                    "glob": { "type": "string", "description": "Only files matching this glob, such as *.ts or src/**/*.rs" }
                },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let pattern = arg_str(args, "pattern")?;
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| ToolError::BadArgs(format!("bad pattern: {e}")))?;
        let start = resolve(
            &ctx.root,
            args.get("path").and_then(Value::as_str).unwrap_or("."),
        )?;

        let mut walk = WalkBuilder::new(&start);
        walk.hidden(true).max_filesize(Some(MAX_FILE_BYTES));
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            let mut over = OverrideBuilder::new(&start);
            over.add(glob)
                .map_err(|e| ToolError::BadArgs(format!("bad glob: {e}")))?;
            walk.overrides(
                over.build()
                    .map_err(|e| ToolError::BadArgs(e.to_string()))?,
            );
        }

        let mut out = String::new();
        let mut matches = 0;
        let mut files = 0;
        'files: for entry in walk.build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            if bytes.contains(&0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            let shown = display(&ctx.root, entry.path());
            let mut hit = false;
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                hit = true;
                if matches == MAX_MATCHES {
                    out.push_str(
                        "[... more matches not shown; narrow the search]
",
                    );
                    break 'files;
                }
                let line = line.trim();
                let line: String = if line.len() > MAX_LINE {
                    format!("{}…", line.chars().take(MAX_LINE).collect::<String>())
                } else {
                    line.to_string()
                };
                out.push_str(&format!("{shown}:{}: {line}\n", i + 1));
                matches += 1;
            }
            if hit {
                files += 1;
            }
        }
        if matches == 0 {
            return Ok("no matches".into());
        }
        out.push_str(&format!("[{matches} matches in {files} files]\n"));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_lines_and_honours_glob() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        std::fs::create_dir_all(ctx.root.join("src")).unwrap();
        std::fs::write(ctx.root.join("src/a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
        std::fs::write(ctx.root.join("notes.md"), "FN main is here\n").unwrap();
        let out = Search
            .run(&json!({"pattern": "fn main"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("src/a.rs:1: fn main() {}"), "{out}");
        assert!(out.contains("notes.md:1: FN main is here"), "{out}");
        assert!(out.ends_with("[2 matches in 2 files]\n"));
        let out = Search
            .run(&json!({"pattern": "main", "glob": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "src/a.rs:1: fn main() {}\n[1 matches in 1 files]\n");
        assert_eq!(
            Search.run(&json!({"pattern": "zzz"}), &ctx).await.unwrap(),
            "no matches"
        );
        assert!(matches!(
            Search.run(&json!({"pattern": "("}), &ctx).await,
            Err(ToolError::BadArgs(_))
        ));
    }
}
