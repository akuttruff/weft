//! `server`: a dev server weft starts, owns and stops, so the worker can look
//! at a change in a browser without touching the services on the machine.
//! The port is one weft picks free, never a service's; the server runs in a
//! process group of its own and the whole group is stopped when the task
//! ends (or the call that started it is stopped).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use super::bash::{forbidden, left_running};
use super::{Ctx, Tool, ToolError, arg_u64};
use crate::lm::ToolSpec;

/// How long a server has to answer on its port after it starts.
const START_S: u64 = 90;
/// Servers one task may have running at once.
const MAX_RUNNING: usize = 2;
/// Output kept per server: the end of it.
const KEEP_BYTES: usize = 64_000;
const LOOK_TIMEOUT_S: u64 = 60;
/// The narrowest page headless Chrome lays out, whatever its window.
const MIN_WIDTH: u64 = 500;
const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/// The servers this task started. Dropping one stops it.
pub type Servers = Arc<Mutex<Vec<Running>>>;

pub struct Running {
    pub id: usize,
    pub port: u16,
    pub command: String,
    out: Arc<Mutex<String>>,
    /// The process group: the shell and everything it started (npm, node).
    group: Option<u32>,
    _child: tokio::process::Child,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(group) = self.group {
            stop_group(group);
        }
    }
}

/// Ask the group to stop, and make sure of it a moment later.
fn stop_group(group: u32) {
    let pgid = group as libc::pid_t;
    // Rust: `unsafe` because libc calls are; these only send signals.
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    });
}

pub struct Server;

#[async_trait]
impl Tool for Server {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "server".into(),
            description: "Run the project's dev server to see a change in a browser. `start` runs it \
                          on a free port weft picks (`{port}` in the command stands for it) and waits \
                          until it answers; `look` saves a screenshot of a page for `see_image`; \
                          `logs` shows its output; `stop` ends it. weft stops every server when the \
                          task ends. This is not how changes go live; it never touches the services \
                          on the machine."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["start", "look", "logs", "stop"] },
                    "command": { "type": "string", "description": "start: the shell line, with {port} where the port goes; the project's own when left out" },
                    "path": { "type": "string", "description": "look: the page, as a URL path (default /)" },
                    "width": { "type": "integer", "description": "look: window width in px (default 1280; 390 for a phone)" },
                    "height": { "type": "integer", "description": "look: window height in px (default 900; 844 for a phone)" },
                    "id": { "type": "integer", "description": "look, logs, stop: which server (default the latest)" }
                },
                "required": ["action"]
            }),
        }
    }

    fn approval(&self, args: &Value) -> Option<String> {
        if action(args) != "start" {
            return None;
        }
        Some(match args.get("command").and_then(Value::as_str) {
            Some(c) => format!("start a server: `{c}`"),
            None => "start the project's server".into(),
        })
    }

    async fn run(&self, args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
        match action(args) {
            "start" => start(args, ctx).await,
            "look" => look(args, ctx).await,
            "logs" => {
                let servers = ctx.servers.lock().unwrap();
                let s = pick(&servers, args)?;
                Ok(format!(
                    "Server {} (`{}`), the end of its output:\n{}",
                    s.id,
                    s.command,
                    last_lines(&s.out.lock().unwrap(), 60)
                ))
            }
            "stop" => {
                let mut servers = ctx.servers.lock().unwrap();
                let id = pick(&servers, args)?.id;
                servers.retain(|s| s.id != id);
                Ok(format!("Stopped server {id}."))
            }
            other => Err(ToolError::BadArgs(format!(
                "no action `{other}`: start, look, logs or stop"
            ))),
        }
    }
}

fn action(args: &Value) -> &str {
    args.get("action").and_then(Value::as_str).unwrap_or("")
}

fn pick<'a>(servers: &'a [Running], args: &Value) -> Result<&'a Running, ToolError> {
    let found = match arg_u64(args, "id")? {
        Some(id) => servers.iter().find(|s| s.id as u64 == id),
        None => servers.last(),
    };
    found.ok_or_else(|| ToolError::Failed("no such server is running; `start` one first".into()))
}

async fn start(args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .or(ctx.project.serve.as_deref())
        .ok_or_else(|| {
            ToolError::BadArgs(
                "give a `command` with {port} where the port goes (`npx vite --port {port} --strictPort`); \
                 this project has no `serve` line"
                    .into(),
            )
        })?;
    if !command.contains("{port}") {
        return Err(ToolError::BadArgs(
            "put {port} where the port goes (`npx vite --port {port} --strictPort`): weft picks a free \
             one, so no service's port is taken"
                .into(),
        ));
    }
    if let Some(bad) = forbidden(command) {
        return Err(ToolError::Refused(format!("`{bad}` is not allowed here")));
    }
    if let Some(why) = left_running(command) {
        return Err(ToolError::Refused(format!(
            "{why}; give the server's own command, which weft keeps running"
        )));
    }
    if ctx.servers.lock().unwrap().len() >= MAX_RUNNING {
        return Err(ToolError::Refused(format!(
            "{MAX_RUNNING} servers are running; `stop` one first"
        )));
    }

    let port = free_port()?;
    let line = command.replace("{port}", &port.to_string());
    let mut child = tokio::process::Command::new("/bin/zsh")
        .arg("-c")
        .arg(&line)
        .current_dir(&ctx.root)
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let out = Arc::new(Mutex::new(String::new()));
    if let Some(s) = child.stdout.take() {
        tokio::spawn(collect(s, out.clone()));
    }
    if let Some(s) = child.stderr.take() {
        tokio::spawn(collect(s, out.clone()));
    }
    let id = ctx
        .servers
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.id)
        .max()
        .unwrap_or(0)
        + 1;
    let mut running = Running {
        id,
        port,
        command: line.clone(),
        out: out.clone(),
        group: child.id(),
        _child: child,
    };

    // Ready when something answers on the port; the server is dropped, and
    // so stopped, on every way out but that one.
    let began = Instant::now();
    loop {
        if answers(port) {
            break;
        }
        if let Ok(Some(status)) = running._child.try_wait() {
            return Err(ToolError::Failed(format!(
                "`{line}` ended ({status}) before it answered on port {port}:\n{}",
                last_lines(&out.lock().unwrap(), 40)
            )));
        }
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Failed("stopped".into()));
        }
        if began.elapsed() > Duration::from_secs(START_S) {
            return Err(ToolError::Failed(format!(
                "nothing answered on port {port} after {START_S}s, so it was stopped; check that \
                 the command uses {{port}} as its port. Its output:\n{}",
                last_lines(&out.lock().unwrap(), 40)
            )));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let shown = last_lines(&out.lock().unwrap(), 20);
    ctx.servers.lock().unwrap().push(running);
    Ok(format!(
        "Server {id} is running at http://localhost:{port} (`{line}`); weft stops it when the task \
         ends. Its output so far:\n{shown}"
    ))
}

/// A screenshot of one page, saved where `see_image` can read it.
async fn look(args: &Value, ctx: &Ctx) -> Result<String, ToolError> {
    let port = pick(&ctx.servers.lock().unwrap(), args)?.port;
    let path = args.get("path").and_then(Value::as_str).unwrap_or("/");
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let width = arg_u64(args, "width")?.unwrap_or(1280).clamp(320, 3840);
    let height = arg_u64(args, "height")?.unwrap_or(900).clamp(320, 4000);
    let url = format!("http://localhost:{port}{path}");

    let chrome = std::env::var("CHROME").unwrap_or_else(|_| CHROME.into());
    if !std::path::Path::new(&chrome).exists() {
        return Err(ToolError::Failed(format!(
            "no Chrome at {chrome}; set CHROME to its path"
        )));
    }
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let shot: PathBuf = std::env::temp_dir().join(format!("weft-look-{stamp}.png"));
    // Headless Chrome lays a page out at least MIN_WIDTH wide, whatever the
    // window: a phone-width look is the page in a frame that wide, centred
    // in the window, and the picture cut down to the frame.
    let narrow = width < MIN_WIDTH;
    let frame = std::env::temp_dir().join(format!("weft-look-{stamp}.html"));
    let target = if narrow {
        std::fs::write(&frame, frame_page(&url, width, height))?;
        format!("file://{}", frame.display())
    } else {
        url.clone()
    };
    // No `--user-data-dir`: with a fresh profile Chrome never finishes.
    let ran = tokio::time::timeout(
        Duration::from_secs(LOOK_TIMEOUT_S),
        tokio::process::Command::new(&chrome)
            .args([
                "--headless=new",
                "--disable-gpu",
                "--hide-scrollbars",
                "--virtual-time-budget=8000",
            ])
            .arg(format!("--window-size={},{height}", width.max(MIN_WIDTH)))
            .arg(format!("--screenshot={}", shot.display()))
            .arg(&target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    if narrow {
        let _ = std::fs::remove_file(&frame);
    }
    let output = match ran {
        Ok(r) => r?,
        Err(_) => {
            return Err(ToolError::Failed(format!(
                "Chrome took over {LOOK_TIMEOUT_S}s on {url}"
            )));
        }
    };
    if !shot.exists() {
        return Err(ToolError::Failed(format!(
            "Chrome saved no screenshot of {url}:\n{}",
            last_lines(&String::from_utf8_lossy(&output.stderr), 20)
        )));
    }
    if narrow {
        // `sips` crops about the centre, where the frame is.
        let cut = tokio::process::Command::new("/usr/bin/sips")
            .args(["-c", &height.to_string(), &width.to_string()])
            .arg(&shot)
            .output()
            .await?;
        if !cut.status.success() {
            return Err(ToolError::Failed(format!(
                "sips could not cut the screenshot to {width}px:\n{}",
                String::from_utf8_lossy(&cut.stderr)
            )));
        }
    }
    Ok(format!(
        "Saved {} ({url} at {width}x{height}). Call `see_image` with that path and a question \
         about what the change should show; `logs` has the server's side.",
        shot.display()
    ))
}

/// A page holding `url` in a frame `width` wide, centred.
fn frame_page(url: &str, width: u64, height: u64) -> String {
    let url = url.replace('"', "%22");
    format!(
        "<!doctype html><body style=\"margin:0\"><iframe src=\"{url}\" style=\"border:0;display:block;\
         margin:0 auto;width:{width}px;height:{height}px\"></iframe></body>"
    )
}

/// A port nothing is listening on now.
fn free_port() -> Result<u16, ToolError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

/// Whether something answers on the port, over IPv4 or IPv6: Node binds
/// `localhost` to either.
fn answers(port: u16) -> bool {
    let wait = Duration::from_millis(200);
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|a| TcpStream::connect_timeout(a, wait).is_ok())
}

/// Keep a pipe's lines, the end of them, as they come.
async fn collect(pipe: impl AsyncRead + Unpin, out: Arc<Mutex<String>>) {
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut o = out.lock().unwrap();
        o.push_str(&line);
        o.push('\n');
        if o.len() > KEEP_BYTES {
            let mut cut = o.len() - KEEP_BYTES;
            while !o.is_char_boundary(cut) {
                cut += 1;
            }
            o.drain(..cut);
        }
    }
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return "(no output)\n".into();
    }
    let mut out = lines[lines.len().saturating_sub(n)..].join("\n");
    out.push('\n');
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

    /// A server in a line of Python: answers on the port it is given.
    const PY: &str = "python3 -m http.server {port} --bind 127.0.0.1";

    #[tokio::test]
    async fn starts_on_a_picked_port_and_stops_when_dropped() {
        let (_d, ctx) = ctx();
        let said = Server
            .run(&json!({"action": "start", "command": PY}), &ctx)
            .await
            .unwrap();
        assert!(
            said.contains("Server 1 is running at http://localhost:"),
            "{said}"
        );
        let port = ctx.servers.lock().unwrap()[0].port;
        assert!(answers(port));

        let logs = Server.run(&json!({"action": "logs"}), &ctx).await.unwrap();
        assert!(logs.contains("Server 1"), "{logs}");

        Server.run(&json!({"action": "stop"}), &ctx).await.unwrap();
        let began = Instant::now();
        while answers(port) && began.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!answers(port), "the server outlived its stop");
    }

    #[tokio::test]
    async fn a_server_that_dies_says_why() {
        let (_d, ctx) = ctx();
        let err = Server
            .run(
                &json!({"action": "start", "command": "echo no such app {port}; exit 2"}),
                &ctx,
            )
            .await
            .unwrap_err();
        let err = err.to_string();
        assert!(
            err.contains("before it answered") && err.contains("no such app"),
            "{err}"
        );
        assert!(ctx.servers.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_port_is_weft_s_and_nothing_is_left_behind() {
        let (_d, ctx) = ctx();
        for (command, says) in [
            ("npx vite --port 8210", "{port}"),
            ("npx vite --port {port} &", "leave processes running"),
            ("kill 1; npx vite --port {port}", "stop processes"),
        ] {
            let err = Server
                .run(&json!({"action": "start", "command": command}), &ctx)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{command}: {err}");
        }
        let err = Server
            .run(&json!({"action": "start"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no `serve` line"), "{err}");
    }

    #[test]
    fn a_phone_width_page_sits_centred_in_its_frame() {
        let page = frame_page("http://localhost:5/a?b=\"c\"", 390, 844);
        assert!(
            page.contains("src=\"http://localhost:5/a?b=%22c%22\""),
            "{page}"
        );
        assert!(
            page.contains("margin:0 auto;width:390px;height:844px"),
            "{page}"
        );
    }

    #[test]
    fn only_start_asks_first() {
        assert!(
            Server
                .approval(&json!({"action": "start", "command": PY}))
                .is_some()
        );
        assert!(Server.approval(&json!({"action": "look"})).is_none());
        assert!(Server.approval(&json!({"action": "stop"})).is_none());
    }
}
