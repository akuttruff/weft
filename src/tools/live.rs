//! `live`: what the running app actually returns and stores, read-only. A
//! bug report is about what the person saw; this lets the planner (and the
//! reviewer) check it before explaining it from the code. Only the project's
//! own app (`app` in its config) and database (`database`) can be read.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Ctx, Tool, ToolError, arg_str, cap};
use crate::lm::ToolSpec;

const OUT_LINES: usize = 200;
const OUT_BYTES: usize = 12_000;
const TIMEOUT_S: u64 = 15;

pub struct Live;

#[async_trait]
impl Tool for Live {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "live".into(),
            description: "Read what the running app returns and what its database holds, without \
                          changing anything. `get` fetches a path from the app (`/api/day/2026-11-02`); \
                          `sql` runs one SELECT (or WITH, or PRAGMA) on its database, read-only; \
                          `tables` lists the tables. Use it to check a reported bug against the real \
                          data before explaining it from the code."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get", "sql", "tables"] },
                    "path": { "type": "string", "description": "get: the path and query, starting with /" },
                    "pick": { "type": "string", "description": "get: only this part of a JSON answer, as a JSON pointer (`/alcohol`, `/sessions/0/items`)" },
                    "query": { "type": "string", "description": "sql: one read-only statement" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        let out = match arg_str(args, "action")? {
            "get" => get(arg_str(args, "path")?, pick(args), ctx).await?,
            "sql" => sql(arg_str(args, "query")?, ctx).await?,
            "tables" => {
                sql(
                    "SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name",
                    ctx,
                )
                .await?
            }
            other => {
                return Err(ToolError::BadArgs(format!(
                    "no action `{other}`: get, sql or tables"
                )));
            }
        };
        Ok(cap(&out, OUT_LINES, OUT_BYTES))
    }
}

/// One GET from the project's app; the path cannot leave it.
async fn get(path: &str, pick: Option<&str>, ctx: &Ctx) -> Result<String, ToolError> {
    let base = ctx.project.app.as_deref().ok_or_else(|| {
        ToolError::Failed("this project has no `app` line in weft's config".into())
    })?;
    let url = url_for(base, path)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_S))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let res = client
        .get(&url)
        .send()
        .await
        .map_err(|e| ToolError::Failed(format!("{url}: {e}")))?;
    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| ToolError::Failed(format!("{url}: {e}")))?;
    // JSON reads better a field to a line, and only the part asked for.
    let body = match (serde_json::from_str::<Value>(&text), pick) {
        (Ok(v), Some(p)) => {
            let part = v.pointer(p).ok_or_else(|| {
                ToolError::Failed(format!(
                    "{url} has nothing at {p}; its top-level keys: {}",
                    keys(&v)
                ))
            })?;
            serde_json::to_string_pretty(part).unwrap_or_default()
        }
        (Ok(v), None) => serde_json::to_string_pretty(&v).unwrap_or(text),
        (Err(_), _) => text,
    };
    let at = pick.map(|p| format!(" at {p}")).unwrap_or_default();
    Ok(format!("GET {url} → {status}{at}\n{body}"))
}

fn pick(args: &Value) -> Option<&str> {
    args.get("pick")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

fn keys(v: &Value) -> String {
    match v.as_object() {
        Some(o) => o.keys().map(String::as_str).collect::<Vec<_>>().join(", "),
        None => "none (not an object)".into(),
    }
}

/// The app's base and a path under it: a path that would reach another
/// host (`//elsewhere`, `@host`) or isn't a path is refused.
fn url_for(base: &str, path: &str) -> Result<String, ToolError> {
    let path = path.trim();
    if !path.starts_with('/') || path.starts_with("//") || path.contains('@') || path.contains('\\')
    {
        return Err(ToolError::BadArgs(
            "`path` is a path on the app, starting with one /".into(),
        ));
    }
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// One statement, read-only: sqlite opens the file read-only and in safe
/// mode, which also turns off the dot-commands that reach outside it.
async fn sql(query: &str, ctx: &Ctx) -> Result<String, ToolError> {
    let db = database(ctx)?;
    let query = query.trim().trim_end_matches(';').trim();
    let first = query
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(first.as_str(), "select" | "with" | "pragma") || query.contains(';') {
        return Err(ToolError::Refused(
            "`live` runs one SELECT, WITH or PRAGMA statement".into(),
        ));
    }
    let ran = tokio::time::timeout(
        Duration::from_secs(TIMEOUT_S),
        tokio::process::Command::new("sqlite3")
            .args(["-readonly", "-safe", "-header", "-column"])
            .arg(&db)
            .arg(query)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| ToolError::Failed(format!("the query took over {TIMEOUT_S}s")))??;
    let mut out = String::from_utf8_lossy(&ran.stdout).into_owned();
    out.push_str(&String::from_utf8_lossy(&ran.stderr));
    if !ran.status.success() {
        return Err(ToolError::Failed(out));
    }
    Ok(if out.trim().is_empty() {
        "(no rows)\n".into()
    } else {
        out
    })
}

/// The database file: relative to the project, or from `~/`.
fn database(ctx: &Ctx) -> Result<PathBuf, ToolError> {
    let given = ctx.project.database.as_deref().ok_or_else(|| {
        ToolError::Failed("this project has no `database` line in weft's config".into())
    })?;
    let path = match given.strip_prefix("~/") {
        Some(rest) => std::env::home_dir().unwrap_or_default().join(rest),
        None => ctx.root.join(given),
    };
    if !path.exists() {
        return Err(ToolError::Failed(format!(
            "no database at {}",
            path.display()
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx(app: Option<&str>, database: Option<&str>) -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = Ctx::new(dir.path().to_path_buf());
        ctx.project = Arc::new(crate::config::Project {
            app: app.map(Into::into),
            database: database.map(Into::into),
            ..Default::default()
        });
        (dir, ctx)
    }

    #[test]
    fn a_path_stays_on_the_app() {
        assert_eq!(
            url_for("http://127.0.0.1:8210/", "/api/day/2026-11-02").unwrap(),
            "http://127.0.0.1:8210/api/day/2026-11-02"
        );
        for bad in ["api/day", "//evil.com/x", "/@evil.com", "http://x/"] {
            assert!(url_for("http://127.0.0.1:8210", bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn reads_the_database_and_refuses_to_change_it() {
        let (dir, ctx) = ctx(None, Some("app.sqlite"));
        let db = dir.path().join("app.sqlite");
        let made = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("CREATE TABLE drinks (date TEXT); INSERT INTO drinks VALUES ('2026-11-01');")
            .status()
            .unwrap();
        assert!(made.success());

        let rows = Live
            .run(
                &json!({"action": "sql", "query": "select date from drinks;"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(rows.contains("2026-11-01"), "{rows}");
        let tables = Live.run(&json!({"action": "tables"}), &ctx).await.unwrap();
        assert!(tables.contains("drinks"), "{tables}");

        for bad in [
            "delete from drinks",
            "select 1; delete from drinks",
            ".shell touch x",
        ] {
            let err = Live
                .run(&json!({"action": "sql", "query": bad}), &ctx)
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::Refused(_)), "{bad}: {err}");
        }
        // Read-only even where the first word passes.
        let err = Live
            .run(
                &json!({"action": "sql", "query": "pragma user_version = 3"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }

    /// An app that answers every request with one JSON body.
    async fn app(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        base
    }

    #[tokio::test]
    async fn picks_the_part_of_the_answer_asked_for() {
        let base = app(r#"{"alcohol":{"weekTotal":8},"water":{"ounces":40}}"#).await;
        let (_d, ctx) = ctx(Some(&base), None);
        let got = Live
            .run(
                &json!({"action": "get", "path": "/api/day/2026-11-01", "pick": "/alcohol"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            got.contains("at /alcohol") && got.contains("\"weekTotal\": 8"),
            "{got}"
        );
        assert!(!got.contains("ounces"), "{got}");
        let err = Live
            .run(
                &json!({"action": "get", "path": "/", "pick": "/nope"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("alcohol, water"), "{err}");
    }

    #[tokio::test]
    async fn says_what_is_missing_from_the_config() {
        let (_d, ctx) = ctx(None, None);
        let err = Live
            .run(&json!({"action": "get", "path": "/"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no `app` line"), "{err}");
    }
}
