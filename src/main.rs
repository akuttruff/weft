//! `weft`: run it in a repository to start a session; `weft run "task"` does
//! one task and exits. `probe` and `tool` are for poking at the pieces.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use weft::agent::worker::{Worker, WorkerError};
use weft::agent::{AgentEvent, Approver, Decision, Role, Yes, prompts};
use weft::config::Config;
use weft::lm::chat::ChatModel;
use weft::lm::raw::RawModel;
use weft::lm::{Endpoint, Message, Request, StreamEvent, ToolCall, ToolSpec};
use weft::tools::{Ctx, Registry};

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
    /// Give the worker one task in the current folder and exit when it is done.
    Run { task: String },
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config_file = match args.config {
        Some(p) => p,
        None => Config::path()?,
    };
    let config = Config::load(&config_file)?;

    match args.command {
        None => {
            Session::new(&config, args.yes, args.think)?
                .interactive()
                .await
        }
        Some(Command::Run { task }) => {
            let mut session = Session::new(&config, args.yes, args.think)?;
            session.run(task).await
        }
        Some(Command::Probe { question }) => probe(&config, question, args.think).await,
        Some(Command::Tool { name, args }) => tool(name, args).await,
    }
}

/// One conversation with the worker in one folder. Tasks given one after
/// another share the conversation, so "now do the same for X" works.
struct Session {
    model: RawModel,
    registry: Registry,
    ctx: Ctx,
    approver: Box<dyn Approver>,
    allowed: std::collections::HashSet<String>,
    messages: Vec<Message>,
}

impl Session {
    fn new(config: &Config, yes: bool, think: bool) -> Result<Self> {
        let endpoint = Endpoint {
            url: config.lm_studio.url.clone(),
            api_token: config.lm_studio.api_token.clone(),
            model: config.worker.model.clone(),
        };
        let approver: Box<dyn Approver> = if yes {
            Box::new(Yes)
        } else {
            Box::new(Terminal)
        };
        Ok(Self {
            model: RawModel::new(endpoint, think || config.worker.thinking),
            registry: Registry::builtin(),
            ctx: Ctx {
                root: std::env::current_dir()?,
            },
            approver,
            allowed: Default::default(),
            messages: vec![Message::System(prompts::WORKER.into())],
        })
    }

    /// Read tasks from the terminal until `/quit` or Ctrl-D.
    async fn interactive(mut self) -> Result<()> {
        println!(
            "\x1b[1mweft\x1b[0m · {} in {}\n\x1b[2m/think toggles the worker's thinking · /clear starts a fresh conversation · /quit leaves\x1b[0m\n",
            self.model.endpoint.model,
            self.ctx.root.display()
        );
        loop {
            let Some(line) = read_line("\x1b[1m>\x1b[0m ").await else {
                break;
            };
            let line = line.trim().to_string();
            match line.as_str() {
                "" => continue,
                "/quit" | "/exit" | "/q" => break,
                "/think" => {
                    self.model.thinking = !self.model.thinking;
                    println!(
                        "\x1b[2mthinking {}\x1b[0m",
                        if self.model.thinking { "on" } else { "off" }
                    );
                }
                "/clear" => {
                    self.messages.truncate(1);
                    println!("\x1b[2mfresh conversation\x1b[0m");
                }
                _ if line.starts_with('/') => println!("\x1b[2munknown command {line}\x1b[0m"),
                _ => {
                    if let Err(e) = self.run(line).await {
                        println!("\x1b[31m{e:#}\x1b[0m");
                    }
                }
            }
            println!();
        }
        Ok(())
    }

    /// One task: run the worker on it and print what happens.
    async fn run(&mut self, task: String) -> Result<()> {
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

        let mut worker = Worker {
            model: &self.model,
            registry: &self.registry,
            ctx: &self.ctx,
            approver: self.approver.as_ref(),
            events,
            cancel,
            allowed: std::mem::take(&mut self.allowed),
        };
        self.messages.push(Message::User(task));
        let began = Instant::now();
        let result = worker.run(&mut self.messages).await;
        self.allowed = std::mem::take(&mut worker.allowed);
        drop(worker);
        on_ctrl_c.abort();
        let _ = printer.await;

        let outcome = match result {
            Ok(o) => o,
            Err(WorkerError::Cancelled) => {
                println!("\n\x1b[2mstopped\x1b[0m");
                // The half-finished turn stays in the conversation only if it is whole.
                if matches!(self.messages.last(), Some(Message::User(_))) {
                    self.messages.pop();
                }
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        println!(
            "\n\x1b[2m{} rounds · {:.0}s · {} prompt + {} completion tokens · touched: {}\x1b[0m",
            outcome.rounds,
            began.elapsed().as_secs_f32(),
            outcome.usage.prompt_tokens,
            outcome.usage.completion_tokens,
            if outcome.touched.is_empty() {
                "nothing".to_string()
            } else {
                outcome.touched.join(", ")
            }
        );
        Ok(())
    }
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
    while let Some(evt) = rx.recv().await {
        match evt {
            AgentEvent::TurnStart { .. } => {}
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
            AgentEvent::ToolStart { summary, .. } => {
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
                at_line_start = true;
            }
            AgentEvent::TurnEnd {
                role,
                usage,
                seconds,
            } => {
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

async fn tool(name: String, args: Option<String>) -> Result<()> {
    let arguments = match args {
        Some(a) => serde_json::from_str(&a).context("the arguments are not JSON")?,
        None => json!({}),
    };
    let ctx = Ctx {
        root: std::env::current_dir()?,
    };
    let registry = Registry::builtin();
    let call = ToolCall {
        id: "call_1".into(),
        name,
        arguments,
    };
    if let Some(what) = registry.approval(&call) {
        println!("\x1b[2m(would ask to {what})\x1b[0m");
    }
    match registry.run(&call, &ctx).await {
        Ok(out) => print!("{out}"),
        Err(e) => println!("error: {e}"),
    }
    Ok(())
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
