//! `weft`: for now, `weft probe` tries both models against LM Studio.

use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use weft::config::Config;
use weft::lm::chat::ChatModel;
use weft::lm::raw::RawModel;
use weft::lm::{Endpoint, Message, Request, StreamEvent, ToolSpec};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The config file [default: ~/.config/weft/config.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ask both models one question and show how each answers.
    Probe {
        /// The question [default: a small one that invites a tool call]
        question: Option<String>,
        /// Let the worker think too
        #[arg(long)]
        think: bool,
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
        Command::Probe { question, think } => probe(&config, question, think).await,
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
    run(
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
    run(
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
async fn run<F, Fut>(start: F, who: &str) -> Result<()>
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
