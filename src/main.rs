//! `weft`: run it in a repository and you are in a session. `weft run "task"`
//! does one task on a plain line printer and exits; `probe` and `tool` poke
//! at the pieces.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use weft::agent::tandem::Mode;
use weft::agent::worker::WorkerError;
use weft::agent::{AgentEvent, Approver, Decision, Role, Yes};
use weft::config::Config;
use weft::lm::chat::ChatModel;
use weft::lm::raw::RawModel;
use weft::lm::{Endpoint, Message, Request, StreamEvent, ToolCall, ToolSpec};
use weft::mcp::McpConfig;
use weft::session::Session;
use weft::tools::{Ctx, Registry};
use weft::transcript;
use weft::ui::diff;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The config file [default: ~/.config/weft/config.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Approve every tool call without asking
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    /// Let the worker think before each step
    #[arg(long, global = true)]
    think: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// One task in the current folder, printed line by line, then exit.
    Run {
        task: String,
        /// Continue an earlier session: its id, or `last`
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Pick up this folder's last session, or the one with this id.
    Resume { id: Option<String> },
    /// Ask both models one question and show how each answers.
    Probe {
        /// The question [default: a small one that invites a tool call]
        question: Option<String>,
    },
    /// Run one tool by hand, as the worker would: `weft tool search '{"pattern":"fn main"}'`.
    Tool {
        name: String,
        /// The arguments as JSON (default: {})
        args: Option<String>,
    },
    /// Start the MCP servers in ~/.config/weft/mcp.json and list their tools.
    Mcp,
    /// List this folder's earlier sessions.
    Sessions,
    /// Write a starter config and a WEFT.md for this folder, where missing.
    Init,
}

const CONFIG_TEMPLATE: &str = r#"# weft: which model plays which role. See the README for every setting.
[lm_studio]
url = "http://127.0.0.1:1234"
# api_token = "..."           # or export LM_API_TOKEN

[planner]
model = "qwen/qwen3.8-27b"    # plans and reviews, on the chat route
plan_effort = "medium"        # low | medium | xhigh
review_effort = "medium"
when = "auto"                 # auto | always | never
review_rounds = 1

[worker]
model = "qwen3.5-122b-a10b"   # runs the tools, on the raw route
thinking = false

[scout]
# model = "qwen/qwen3.5-9b"   # answers "where is X?"; default: the worker's model

[compact]
at = 48000                    # fold the conversation at this many tokens; 0 never
window = 65536                # the worker's context window

[approve]                     # tool calls that never ask
commands = ["cargo test", "cargo build", "cargo clippy", "git status", "git diff", "git log", "ls"]
"#;

const WEFT_TEMPLATE: &str = r#"# Notes for weft

Standing instructions for every task in this project. Keep it short: this file
is in every prompt.

## Running things
- Tests:
- Build or type-check:

## Rules
- Never:
- Always:

## Conventions
- Commit messages:
- Code style:
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config_file = match args.config {
        Some(p) => p,
        None => Config::path()?,
    };
    // `init` writes the config, so it cannot wait for one to load.
    if matches!(args.command, Some(Command::Init)) {
        return init(&config_file);
    }
    let config = Config::load(&config_file)?;
    let think = args.think || config.worker.thinking;

    match args.command {
        None | Some(Command::Resume { .. }) => {
            let mut session = Session::new(&config, std::env::current_dir()?);
            session.attach_mcp(connect_mcp().await?);
            check_models(&config, &mut session).await;
            let resume = match args.command {
                Some(Command::Resume { id }) => {
                    Some(saved(&session, id.as_deref().unwrap_or("last"))?)
                }
                _ => {
                    start_transcript(&session);
                    None
                }
            };
            let session = Arc::new(session);
            let result = weft::ui::run(session.clone(), args.yes, think, resume).await;
            session.finish();
            result
        }
        Some(Command::Run {
            task,
            session: which,
        }) => {
            let mut session = Session::new(&config, std::env::current_dir()?);
            session.attach_mcp(connect_mcp().await?);
            check_models(&config, &mut session).await;
            for warning in &session.warnings {
                eprintln!("\x1b[33m{warning}\x1b[0m");
            }
            match &which {
                Some(which) => {
                    let saved = saved(&session, which)?;
                    session.resume(&saved)?;
                    eprintln!(
                        "\x1b[2mresumed session {} · {} task{}\x1b[0m",
                        saved.id,
                        saved.tasks.len(),
                        if saved.tasks.len() == 1 { "" } else { "s" }
                    );
                }
                None => start_transcript(&session),
            }
            for status in &session.mcp {
                if let Err(e) = &status.tools {
                    eprintln!("\x1b[31mmcp {}: {e}\x1b[0m", status.name);
                }
            }
            let result = run_once(&session, task, args.yes, think).await;
            session.finish();
            result
        }
        Some(Command::Probe { question }) => probe(&config, question, args.think).await,
        Some(Command::Tool { name, args }) => tool(&config, name, args).await,
        Some(Command::Init) => unreachable!("handled before the config loads"),
        Some(Command::Sessions) => {
            let dir = transcript::dir_for(&std::env::current_dir()?)?;
            let all = transcript::list(&dir)?;
            if all.is_empty() {
                println!("no sessions yet for this folder");
            }
            for s in all {
                let first = s.tasks.first().map(String::as_str).unwrap_or("(no task)");
                println!(
                    "\x1b[1m{}\x1b[0m  \x1b[2m{} · {} task{}\x1b[0m  {}",
                    s.id,
                    &s.started[..16.min(s.started.len())],
                    s.tasks.len(),
                    if s.tasks.len() == 1 { "" } else { "s" },
                    first.lines().next().unwrap_or("")
                );
            }
            Ok(())
        }
        Some(Command::Mcp) => {
            for (status, server) in connect_mcp().await? {
                match (&status.tools, server) {
                    (Ok(_), Some(server)) => {
                        println!(
                            "\x1b[1m{}\x1b[0m · {} tools",
                            status.name,
                            server.tools.len()
                        );
                        for t in &server.tools {
                            let first = t
                                .description
                                .as_deref()
                                .unwrap_or("")
                                .lines()
                                .next()
                                .unwrap_or("");
                            println!(
                                "  \x1b[36m{}__{}\x1b[0m  \x1b[2m{first}\x1b[0m",
                                status.name, t.name
                            );
                        }
                    }
                    (Err(e), _) => {
                        println!("\x1b[1m{}\x1b[0m · \x1b[31mfailed: {e}\x1b[0m", status.name)
                    }
                    (Ok(_), None) => unreachable!("a listed server is a connected server"),
                }
            }
            Ok(())
        }
    }
}

/// Write the starter files that are missing, and say which.
fn init(config_file: &PathBuf) -> Result<()> {
    if config_file.exists() {
        println!("config: {} is already there", config_file.display());
    } else {
        if let Some(dir) = config_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(config_file, CONFIG_TEMPLATE)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(config_file, std::fs::Permissions::from_mode(0o600))?;
        }
        println!(
            "config: wrote {}; set the models and the token",
            config_file.display()
        );
    }
    let notes = std::env::current_dir()?.join("WEFT.md");
    if notes.exists() {
        println!("notes:  {} is already there", notes.display());
    } else {
        std::fs::write(&notes, WEFT_TEMPLATE)?;
        println!(
            "notes:  wrote {}; fill in how this project is run",
            notes.display()
        );
    }
    Ok(())
}

/// Warn, at the start, about a configured model LM Studio has not loaded.
async fn check_models(config: &Config, session: &mut Session) {
    let endpoint = Endpoint {
        url: config.lm_studio.url.clone(),
        api_token: config.lm_studio.api_token.clone(),
        model: String::new(),
    };
    let mut wanted = vec![
        config.planner.model.clone(),
        config.worker.model.clone(),
        session.scout_name().to_string(),
    ];
    wanted.dedup();
    match tokio::time::timeout(Duration::from_secs(3), weft::lm::loaded_models(&endpoint)).await {
        Ok(Ok(loaded)) => {
            for model in wanted {
                if !loaded.contains(&model) {
                    session.warnings.push(format!(
                        "{model} is not loaded in LM Studio (loaded: {})",
                        if loaded.is_empty() {
                            "nothing".to_string()
                        } else {
                            loaded.join(", ")
                        }
                    ));
                }
            }
        }
        Ok(Err(e)) => session
            .warnings
            .push(format!("could not list LM Studio's models: {e}")),
        Err(_) => session
            .warnings
            .push("LM Studio did not answer within 3 seconds".into()),
    }
}

/// The earlier session `which` names, read from this folder's transcripts.
fn saved(session: &Session, which: &str) -> Result<transcript::Saved> {
    transcript::find(&transcript::dir_for(&session.ctx.root)?, which)
}

/// Keep a transcript of this session; without one the session still runs.
fn start_transcript(session: &Session) {
    if let Err(e) = session.start_transcript() {
        eprintln!("\x1b[33mnot keeping a transcript: {e:#}\x1b[0m");
    }
}

/// Start the configured MCP servers; a missing config file means none.
async fn connect_mcp() -> Result<Vec<(weft::mcp::Status, Option<weft::mcp::Server>)>> {
    let config = McpConfig::load(&McpConfig::path()?)?;
    Ok(weft::mcp::connect_all(&config).await)
}

/// One task on the plain printer: for scripts and for watching the raw stream.
async fn run_once(session: &Session, task: String, yes: bool, think: bool) -> Result<()> {
    let (events, rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let printer = tokio::spawn(print_events(rx));
    let cancel = CancellationToken::new();
    let on_ctrl_c = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        })
    };
    let approver: Box<dyn Approver> = if yes {
        Box::new(Yes)
    } else {
        Box::new(Terminal)
    };
    let began = Instant::now();
    let result = session
        .run_task(&task, Mode::Auto, think, approver.as_ref(), events, cancel)
        .await;
    on_ctrl_c.abort();
    let _ = printer.await;

    let outcome = match result {
        Ok(o) => o,
        Err(WorkerError::Cancelled) => {
            println!("\n\x1b[2mstopped\x1b[0m");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };
    let review = match (outcome.reviews.last(), outcome.fixes_pending) {
        (None, _) => "",
        (Some(_), true) => " · review: fixes still pending",
        (Some((true, _)), false) => " · review: approved",
        (Some((false, _)), false) => " · review: not approved",
    };
    println!(
        "\n\x1b[2m{} rounds · {:.0}s · planner {} · worker {} tokens · touched: {}{review}\x1b[0m",
        outcome.rounds,
        began.elapsed().as_secs_f32(),
        outcome.planner_usage.prompt_tokens + outcome.planner_usage.completion_tokens,
        outcome.worker_usage.prompt_tokens + outcome.worker_usage.completion_tokens,
        if outcome.touched.is_empty() {
            "nothing".to_string()
        } else {
            outcome.touched.join(", ")
        }
    );
    Ok(())
}

/// A line from the terminal, or `None` at end of input (Ctrl-D) or Ctrl-C.
async fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let read = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    });
    tokio::select! {
        line = read => line.ok().flatten(),
        _ = tokio::signal::ctrl_c() => { println!(); None }
    }
}

/// Asks on the terminal: y / a / n.
struct Terminal;

#[async_trait]
impl Approver for Terminal {
    async fn ask(&self, what: &str) -> Decision {
        let prompt = format!(
            "\n\x1b[33m?\x1b[0m Allow weft to {what}?  [y] once  [a] always this session  [n] no  "
        );
        // Rust: reading stdin blocks the thread, so `read_line` sends it to a
        // blocking task rather than stalling the runtime that is drawing output.
        match read_line(&prompt).await.as_deref().map(str::trim) {
            Some("y") | Some("yes") => Decision::Once,
            Some("a") | Some("always") => Decision::Always,
            _ => Decision::No,
        }
    }
}

/// Draw the agent's events as they come: reasoning dimmed, tool calls as one
/// line each with the first line of their result under it.
async fn print_events(mut rx: tokio::sync::mpsc::Receiver<AgentEvent>) {
    let mut out = std::io::stdout();
    let mut dim = false;
    let mut at_line_start = true;
    // The planner's thinking is long and not worth reading live: it shows as
    // one line that ticks, written over in place, and is wiped when the turn ends.
    let mut ticker: Option<(Instant, usize)> = None;
    // The call whose result is next, so an edit can be printed as a diff.
    let mut last_call = (String::new(), json!(null));
    while let Some(evt) = rx.recv().await {
        match evt {
            AgentEvent::TurnStart {
                role: Role::Planner,
            } => ticker = Some((Instant::now(), 0)),
            AgentEvent::TurnStart { .. } => {}
            AgentEvent::Reasoning {
                role: Role::Planner,
                text,
            } => {
                if let Some((since, chars)) = &mut ticker {
                    *chars += text.len();
                    print!(
                        "\r\x1b[2K\x1b[2m  ✻ planner thinking · {}s · ~{} tokens\x1b[0m",
                        since.elapsed().as_secs(),
                        *chars / 4
                    );
                    at_line_start = false;
                }
            }
            AgentEvent::Phase { name } => {
                if dim {
                    print!("\x1b[0m");
                    dim = false;
                }
                if !at_line_start {
                    println!();
                }
                println!("\x1b[35m✻\x1b[0m \x1b[1m{name}\x1b[0m");
                at_line_start = true;
            }
            // The planner's and reviewer's answers arrive whole as Plan and
            // Review; their streamed text is not printed twice.
            AgentEvent::Text {
                role: Role::Planner,
                ..
            } => {}
            AgentEvent::Plan { text } => {
                if dim {
                    print!("\x1b[0m");
                    dim = false;
                }
                if !at_line_start {
                    println!();
                }
                for line in text.lines() {
                    println!("  \x1b[36m│\x1b[0m {line}");
                }
                at_line_start = true;
            }
            AgentEvent::Review { approved, text } => {
                if dim {
                    print!("\x1b[0m");
                    dim = false;
                }
                if !at_line_start {
                    println!();
                }
                if approved {
                    println!("  \x1b[32m✔ approved\x1b[0m \x1b[2m{text}\x1b[0m");
                } else {
                    println!("  \x1b[33m✗ fixes requested\x1b[0m");
                    for line in text.lines() {
                        println!("    {line}");
                    }
                }
                at_line_start = true;
            }
            AgentEvent::Reasoning { text, .. } => {
                if !dim {
                    print!("\x1b[2m");
                    dim = true;
                }
                print!("{text}");
                at_line_start = text.ends_with('\n');
            }
            AgentEvent::Text { text, .. } => {
                if dim {
                    println!("\x1b[0m");
                    dim = false;
                }
                print!("{text}");
                at_line_start = text.ends_with('\n');
            }
            AgentEvent::ToolStart {
                summary,
                name,
                args,
                ..
            } => {
                last_call = (name, args);
                if dim {
                    print!("\x1b[0m");
                    dim = false;
                }
                if !at_line_start {
                    println!();
                }
                print!("\x1b[32m⏺\x1b[0m {summary}");
                at_line_start = false;
            }
            AgentEvent::ToolDone { ok, output, .. } => {
                let first = output
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(100)
                    .collect::<String>();
                let lines = output.lines().count();
                let more = if lines > 1 {
                    format!(" (+{} lines)", lines - 1)
                } else {
                    String::new()
                };
                let mark = if ok { "\x1b[2m" } else { "\x1b[31m" };
                println!("\n  {mark}⎿ {first}{more}\x1b[0m");
                if ok {
                    print_diff(&last_call.0, &last_call.1, &output);
                }
                at_line_start = true;
            }
            AgentEvent::Trimmed { dropped, tokens } => println!(
                "\x1b[2m  dropped {dropped} earlier tool results to make room (context {tokens} tokens)\x1b[0m"
            ),
            AgentEvent::Compacted {
                before,
                after,
                tasks,
            } => println!(
                "\x1b[2m  compacted: {tasks} earlier tasks folded into a note · context {before} → ~{after} tokens\x1b[0m"
            ),
            AgentEvent::TurnEnd {
                role,
                usage,
                seconds,
            } => {
                if ticker.take().is_some() {
                    print!("\r\x1b[2K");
                    at_line_start = true;
                }
                if dim {
                    print!("\x1b[0m");
                    dim = false;
                }
                if !at_line_start {
                    println!();
                }
                let who = match role {
                    Role::Planner => "planner",
                    Role::Worker => "worker",
                    Role::Scout => "scout",
                };
                println!(
                    "\x1b[2m  {who} {seconds:.1}s · prompt {} · reasoning {} · answer {}\x1b[0m",
                    usage.prompt_tokens,
                    usage.reasoning_tokens,
                    usage
                        .completion_tokens
                        .saturating_sub(usage.reasoning_tokens)
                );
                at_line_start = true;
            }
        }
        let _ = out.flush();
    }
}

async fn tool(config: &Config, name: String, args: Option<String>) -> Result<()> {
    let arguments = match args {
        Some(a) => serde_json::from_str(&a).context("the arguments are not JSON")?,
        None => json!({}),
    };
    let ctx = Ctx::new(std::env::current_dir()?);
    // The scout needs a model, so it comes from a session like the others.
    let mut registry = if name == "scout" {
        Session::new(config, ctx.root.clone()).registry
    } else {
        Registry::builtin()
    };
    let mut servers = Vec::new();
    if name.contains("__") {
        for (_, server) in connect_mcp().await?.into_iter() {
            if let Some(server) = server {
                registry.extend(server.tools());
                servers.push(server);
            }
        }
    }
    let call = ToolCall {
        id: "call_1".into(),
        name,
        arguments,
    };
    if let Some(what) = registry.approval(&call) {
        println!("\x1b[2m(would ask to {what})\x1b[0m");
    }
    match registry.run(&call, &ctx).await {
        Ok(out) => {
            println!("{}", out.trim_end());
            print_diff(&call.name, &call.arguments, &out);
        }
        Err(e) => println!("error: {e}"),
    }
    Ok(())
}

/// The code an edit changed, in plain ANSI, the way the screen shows it.
fn print_diff(tool: &str, args: &serde_json::Value, output: &str) {
    let Some((rows, hidden)) = diff::rows(tool, args, output) else {
        return;
    };
    for row in &rows {
        let (sign, colour) = match row.kind {
            diff::Kind::Removed => ("-", "\x1b[31m"),
            diff::Kind::Added => ("+", "\x1b[32m"),
            diff::Kind::Same => (" ", ""),
        };
        println!(
            "\x1b[2m    {:>4} \x1b[0m{colour}{sign} {}\x1b[0m",
            row.line,
            row.text.replace('\t', "    ")
        );
    }
    if hidden > 0 {
        println!("\x1b[2m           … {hidden} more lines\x1b[0m");
    }
}

async fn probe(config: &Config, question: Option<String>, think: bool) -> Result<()> {
    let question =
        question.unwrap_or_else(|| "What is in the file notes.txt? Use the tool.".to_string());
    let tools = vec![ToolSpec {
        name: "read_file".into(),
        description: "Read a file in the current folder.".into(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path relative to the folder" } },
            "required": ["path"]
        }),
    }];
    let req = Request {
        messages: vec![
            Message::System("You are a terse coding assistant.".into()),
            Message::User(question),
        ],
        tools,
        max_tokens: 2000,
    };

    let endpoint = |model: &str| Endpoint {
        url: config.lm_studio.url.clone(),
        api_token: config.lm_studio.api_token.clone(),
        model: model.to_string(),
    };

    println!(
        "── planner: {} (effort {}) ──",
        config.planner.model,
        config.planner.plan_effort.as_str()
    );
    let planner = ChatModel::new(
        endpoint(&config.planner.model),
        Some(config.planner.plan_effort),
    );
    // Rust: `async move` would move `req` into the block; borrowing first means
    // the block moves only the two references, and `req` is still ours after.
    let (planner, req_ref) = (&planner, &req);
    stream_once(
        |tx| async move { planner.stream(req_ref, &tx).await },
        "planner",
    )
    .await?;

    println!(
        "\n── worker: {} (thinking {}) ──",
        config.worker.model,
        if think { "on" } else { "off" }
    );
    let worker = RawModel::new(endpoint(&config.worker.model), think);
    let (worker, req_ref) = (&worker, &req);
    stream_once(
        |tx| async move { worker.stream(req_ref, &tx).await },
        "worker",
    )
    .await?;
    Ok(())
}

/// Stream one turn, printing reasoning dimmed and text plain, then the numbers.
// Rust: a generic function over the closure `F` that builds the future `Fut`.
// Each model's `stream` has its own type, and this lets one printer serve both.
// The closure takes the sender by value, so the future owns it and closes the
// channel when it finishes, which is what ends the printer's loop.
async fn stream_once<F, Fut>(start: F, who: &str) -> Result<()>
where
    F: FnOnce(weft::lm::Sender) -> Fut,
    Fut: Future<Output = Result<weft::lm::Reply, weft::lm::LmError>>,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(256);
    let began = Instant::now();
    let stream = start(tx);

    // Rust: `tokio::join!` runs the model call and the printer at the same time
    // on one task; the printer ends when the model drops its sender.
    let printer = async {
        let mut out = std::io::stdout();
        let mut in_reasoning = false;
        while let Some(evt) = rx.recv().await {
            match evt {
                StreamEvent::Reasoning(s) => {
                    if !in_reasoning {
                        print!("\x1b[2m");
                        in_reasoning = true;
                    }
                    print!("{s}");
                }
                StreamEvent::Text(s) => {
                    if in_reasoning {
                        print!("\x1b[0m\n\n");
                        in_reasoning = false;
                    }
                    print!("{s}");
                }
                StreamEvent::ToolCall(c) => {
                    if in_reasoning {
                        println!("\x1b[0m");
                        in_reasoning = false;
                    }
                    print!("\n⏺ {}({})", c.name, c.arguments);
                }
            }
            let _ = out.flush();
        }
        if in_reasoning {
            print!("\x1b[0m");
        }
        println!();
    };
    let (reply, ()) = tokio::join!(stream, printer);
    let reply = reply.with_context(|| format!("{who} failed"))?;
    let secs = began.elapsed().as_secs_f32();
    let u = reply.usage;
    println!(
        "\x1b[2m{secs:.1}s · prompt {} · reasoning {} · answer {} · {:.1} tok/s\x1b[0m",
        u.prompt_tokens,
        u.reasoning_tokens,
        u.completion_tokens.saturating_sub(u.reasoning_tokens),
        u.completion_tokens as f32 / secs.max(0.001)
    );
    Ok(())
}
