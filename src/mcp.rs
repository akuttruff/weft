//! MCP servers: other programs' tools, spoken to over stdio, offered to the
//! worker beside the built-in ones as `server__tool`.
//!
//! The servers are listed in `~/.config/weft/mcp.json`, in the shape LM Studio
//! and Claude Desktop use, so an entry can be copied across unchanged. Each is
//! started when weft starts; one that fails is reported and skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::future::join_all;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as ToolInfo};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::process::Command;

use crate::lm::ToolSpec;
use crate::tools::{Ctx, Tool, ToolError, cap};

/// How long a server may take to start and list its tools.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const OUT_LINES: usize = 200;
const OUT_BYTES: usize = 24_000;

#[derive(Debug, Default, Deserialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Where to start it (default: wherever weft runs).
    #[serde(default)]
    pub cwd: Option<String>,
}

impl McpConfig {
    pub fn path() -> Result<PathBuf> {
        let home = std::env::home_dir().context("no home folder")?;
        Ok(home.join(".config/weft/mcp.json"))
    }

    /// The file, or no servers when there is none.
    pub fn load(file: &Path) -> Result<Self> {
        match std::fs::read_to_string(file) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }
}

// Rust: a type alias for a long generic name. `RoleClient` says which side of
// the protocol we are; `()` is the handler for requests the server might send
// us, and we handle none.
type Service = RunningService<RoleClient, ()>;

/// A server weft is talking to.
pub struct Server {
    pub name: String,
    service: Arc<Service>,
    pub tools: Vec<ToolInfo>,
}

/// How a configured server came up, for `/mcp` and `weft mcp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub name: String,
    /// The tools it offers, or why it is not there.
    pub tools: Result<Vec<String>, String>,
}

/// Start every configured server at once and wait for them all.
pub async fn connect_all(config: &McpConfig) -> Vec<(Status, Option<Server>)> {
    let starts = config.servers.iter().map(|(name, cfg)| async move {
        match connect(name, cfg).await {
            Ok(server) => (
                Status {
                    name: name.clone(),
                    tools: Ok(server.tools.iter().map(|t| t.name.to_string()).collect()),
                },
                Some(server),
            ),
            Err(e) => (
                Status {
                    name: name.clone(),
                    tools: Err(format!("{e:#}")),
                },
                None,
            ),
        }
    });
    join_all(starts).await
}

async fn connect(name: &str, cfg: &ServerConfig) -> Result<Server> {
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args).envs(&cfg.env);
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    // The server's stderr would land on top of the screen; the builder sets
    // it, since `new` would reset it to the terminal.
    let (transport, _stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", cfg.command))?;
    let service = tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
        .await
        .map_err(|_| anyhow!("no answer within {}s", CONNECT_TIMEOUT.as_secs()))?
        .context("handshake")?;
    let tools = tokio::time::timeout(CONNECT_TIMEOUT, service.list_all_tools())
        .await
        .map_err(|_| anyhow!("listing tools took over {}s", CONNECT_TIMEOUT.as_secs()))?
        .context("listing tools")?;
    Ok(Server {
        name: name.to_string(),
        service: Arc::new(service),
        tools,
    })
}

impl Server {
    /// The server's tools as weft tools, one `Box<dyn Tool>` each.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        self.tools
            .iter()
            .map(|info| {
                Box::new(McpTool {
                    server: self.name.clone(),
                    service: self.service.clone(),
                    info: info.clone(),
                }) as Box<dyn Tool>
            })
            .collect()
    }
}

/// One tool on one server.
pub struct McpTool {
    server: String,
    service: Arc<Service>,
    info: ToolInfo,
}

/// `server__tool`: the name the model sees.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
}

/// `server__tool` → (server, tool), for summaries.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once("__")
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: tool_name(&self.server, &self.info.name),
            description: self.info.description.as_deref().unwrap_or("").to_string(),
            parameters: Value::Object((*self.info.input_schema).clone()),
        }
    }

    /// Every MCP tool asks the first time; `a` allows it for the session.
    fn approval(&self, _args: &Value) -> Option<String> {
        Some(format!("call {} {}", self.server, self.info.name))
    }

    async fn run(&self, args: &Value, _ctx: &Ctx) -> Result<String, ToolError> {
        let arguments = match args {
            Value::Object(m) => m.clone(),
            Value::Null => Map::new(),
            _ => return Err(ToolError::BadArgs("the arguments must be an object".into())),
        };
        let params = CallToolRequestParams::new(self.info.name.clone()).with_arguments(arguments);
        let result = self
            .service
            .call_tool(params)
            .await
            .map_err(|e| ToolError::Failed(format!("{}: {e}", self.server)))?;
        flatten(result)
    }
}

/// The text of a result, for the model. An error result is a tool error.
fn flatten(result: CallToolResult) -> Result<String, ToolError> {
    let mut parts: Vec<String> = result
        .content
        .iter()
        .map(|c| match c.as_text() {
            Some(t) => t.text.clone(),
            None => "(content that is not text)".to_string(),
        })
        .collect();
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    let text = parts.join("\n");
    if result.is_error.unwrap_or(false) {
        return Err(ToolError::Failed(if text.is_empty() {
            "the server reported an error".into()
        } else {
            text
        }));
    }
    if text.trim().is_empty() {
        return Ok("(no output)".into());
    }
    Ok(cap(&text, OUT_LINES, OUT_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;

    #[test]
    fn parses_the_lm_studio_shape() {
        let text = r#"{"mcpServers": {
            "deep-research": {"command": "/opt/homebrew/bin/node", "args": ["dist/index.js"]},
            "notes": {"command": "node", "env": {"NOTES_DB": "/tmp/n.sqlite"}}
        }}"#;
        let config: McpConfig = serde_json::from_str(text).unwrap();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers["deep-research"].args, ["dist/index.js"]);
        assert_eq!(config.servers["notes"].env["NOTES_DB"], "/tmp/n.sqlite");
        assert!(config.servers["notes"].args.is_empty());
        let missing = McpConfig::load(Path::new("/nonexistent/mcp.json")).unwrap();
        assert!(missing.servers.is_empty());
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(
            tool_name("deep-research", "search"),
            "deep-research__search"
        );
        assert_eq!(
            split_name("deep-research__search"),
            Some(("deep-research", "search"))
        );
        assert_eq!(split_name("read_file"), None);
    }

    #[test]
    fn flattens_text_and_reports_errors() {
        let ok = CallToolResult::success(vec![ContentBlock::text("a"), ContentBlock::text("b")]);
        assert_eq!(flatten(ok).unwrap(), "a\nb\n");
        let err = CallToolResult::error(vec![ContentBlock::text("no results")]);
        assert!(matches!(flatten(err), Err(ToolError::Failed(m)) if m == "no results"));
        let empty = CallToolResult::success(vec![]);
        assert_eq!(flatten(empty).unwrap(), "(no output)");
    }
}
