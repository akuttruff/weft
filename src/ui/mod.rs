//! The screen: an input box at the bottom of the terminal, the transcript
//! scrolling above it in the terminal's own history (no separate screen, so
//! it stays in your scrollback when you leave), a status line while a task
//! runs, and approvals answered with one key.
//!
//! The live part is a small inline viewport that ratatui redraws; finished
//! pieces (a tool call, the plan, an answer) are pushed above it with
//! `insert_before` and never redrawn again.

pub mod diff;
mod draw;
mod events;
mod input;
mod keys;
mod slash;
mod view;

use input::Input;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr;

use crate::agent::compact::Compacted;
use crate::agent::tandem::{Mode, TandemOutcome};
use crate::agent::worker::WorkerError;
use crate::agent::{AgentEvent, Approver, Decision, PlanDecision, Role, Yes};
use crate::commands;
use crate::lm::Usage;
use crate::session::Session;
use crate::transcript::{self, Saved};

/// Rows of the live area: streamed text, status, the input box, the footer.
const LIVE_ROWS: u16 = 8;
const TAIL_ROWS: u16 = 1;
const BOX_ROWS: u16 = 5; // border + 3 lines + border
const TICK: Duration = Duration::from_millis(120);
const SPINNER: [&str; 6] = ["✻", "✢", "✳", "✶", "✻", "✽"];

const COMMANDS: &[(&str, &str, bool)] = &[
    ("/plan", "plan first, then work", true),
    ("/go", "straight to work, no plan", true),
    ("/think", "toggle the worker's thinking", false),
    (
        "/accept",
        "toggle: edits and new files without asking",
        false,
    ),
    ("/clear", "start a fresh conversation", false),
    ("/compact", "fold earlier tasks into a note", false),
    ("/undo", "put back the files the last task changed", false),
    ("/resume", "pick up an earlier session", true),
    ("/memory", "what weft has kept about this project", false),
    ("/cost", "tokens used this session", false),
    ("/mcp", "MCP servers and their tools", false),
    ("/help", "keys and commands", false),
    ("/quit", "leave", false),
];

/// One line of the slash menu: a built-in command or a custom one.
struct MenuItem {
    name: String,
    what: String,
    /// Whether the command wants text after it, so completing it leaves a space.
    takes_task: bool,
}

/// Everything the screen hears about.
enum UiMsg {
    Agent(AgentEvent),
    Ask {
        what: String,
        reply: oneshot::Sender<Decision>,
    },
    /// The plan is on the screen; the user says go, stop, or types notes.
    AskPlan {
        reply: oneshot::Sender<PlanDecision>,
    },
    Done(Result<TandemOutcome, WorkerError>),
    Compacted(Result<Compacted, WorkerError>),
}

/// Approvals asked on screen and answered with a key.
struct UiApprover {
    tx: mpsc::Sender<UiMsg>,
}

#[async_trait]
impl Approver for UiApprover {
    async fn ask(&self, what: &str) -> Decision {
        let (reply, answer) = oneshot::channel();
        if self
            .tx
            .send(UiMsg::Ask {
                what: what.to_string(),
                reply,
            })
            .await
            .is_err()
        {
            return Decision::No;
        }
        answer.await.unwrap_or(Decision::No)
    }

    async fn review_plan(&self, _plan: &str) -> PlanDecision {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(UiMsg::AskPlan { reply }).await.is_err() {
            return PlanDecision::Stop;
        }
        answer.await.unwrap_or(PlanDecision::Stop)
    }
}

/// A task in progress.
struct Running {
    began: Instant,
    phase: &'static str,
    /// The role of the turn under way and how much it has thought, for the status line.
    turn: Option<(Role, usize)>,
    /// The unfinished last line of the worker's answer; whole lines go to the transcript as they complete.
    text: String,
    /// Whether the current answer block has had its `⏺` line yet.
    answer_open: bool,
    cancel: CancellationToken,
}

struct App {
    session: Arc<Session>,
    yes: bool,
    think: bool,
    /// Edits and new files run without asking; shell commands still ask.
    accept_edits: bool,
    input: Input,
    history: Vec<String>,
    history_at: Option<usize>,
    menu_at: usize,
    running: Option<Running>,
    ask: Option<(String, oneshot::Sender<Decision>)>,
    /// A plan waiting for the user's word; the input box takes the notes.
    plan_ask: Option<oneshot::Sender<PlanDecision>>,
    quit_armed: bool,
    quit: bool,
    frame: usize,
    /// Lines waiting to be pushed above the live area, each with the prefix
    /// its continuation lines take when wrapped.
    out: Vec<(Vec<Line<'static>>, Vec<Span<'static>>)>,
    planner_usage: Usage,
    worker_usage: Usage,
    scout_usage: Usage,
    /// The worker's last prompt size: how full its context is.
    context: u32,
    tasks: usize,
    flush_failed: bool,
    /// The tool call whose result is next: its name for the summary line, its
    /// arguments for the diff under an edit.
    last_tool: (String, serde_json::Value),
    /// The last `/resume` listing, so `/resume 2` means something.
    sessions: Vec<Saved>,
}

pub async fn run(
    session: Arc<Session>,
    yes: bool,
    think: bool,
    resume: Option<Saved>,
) -> Result<()> {
    // Rust: `try_init_with_options` returns a Result where `init_with_options`
    // would panic; a pipe or a terminal that does not answer gets a sentence
    // instead of a backtrace.
    let mut terminal = ratatui::try_init_with_options(TerminalOptions {
        viewport: Viewport::Inline(LIVE_ROWS),
    })
    .map_err(|e| {
        anyhow::anyhow!("weft needs a terminal to draw in ({e}); try `weft run \"task\"` instead")
    })?;
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, session, yes, think, resume).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    session: Arc<Session>,
    yes: bool,
    think: bool,
    resume: Option<Saved>,
) -> Result<()> {
    let mut app = App {
        session,
        yes,
        think,
        accept_edits: false,
        input: Input::default(),
        history: Vec::new(),
        history_at: None,
        menu_at: 0,
        running: None,
        ask: None,
        plan_ask: None,
        quit_armed: false,
        quit: false,
        frame: 0,
        out: Vec::new(),
        planner_usage: Usage::default(),
        worker_usage: Usage::default(),
        scout_usage: Usage::default(),
        context: 0,
        tasks: 0,
        flush_failed: false,
        last_tool: (String::new(), serde_json::Value::Null),
        sessions: Vec::new(),
    };
    let (tx, mut rx) = mpsc::channel::<UiMsg>(512);
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(TICK);

    let session_note = match app.session.transcript_id() {
        Some(id) => format!(" · session {id}"),
        None => String::new(),
    };
    app.push(vec![Line::from(vec![
        Span::styled("weft", Style::new().bold()),
        Span::styled(
            format!(
                " · {} plans and reviews · {} works{session_note}",
                app.session.planner_name(),
                app.session.worker_name()
            ),
            Style::new().dim(),
        ),
    ])]);
    app.push(view::note_line(
        "Type a task and press enter. / for commands, esc to stop, ctrl-c twice to leave.",
    ));
    if !app.session.mcp.is_empty() {
        app.push(view::mcp_summary(&app.session.mcp));
    }
    for warning in app.session.warnings.clone() {
        app.push(view::error_line(&format!("⚠ {warning}")));
    }
    match resume {
        Some(saved) => app.resume(saved),
        None => {
            let earlier = app
                .sessions_dir()
                .map(|d| transcript::count(&d))
                .unwrap_or(0);
            // The one just started is in that count.
            if earlier > 1 {
                app.push(view::note_line(&format!(
                    "{} earlier session{} in this folder · /resume lists them",
                    earlier - 1,
                    if earlier == 2 { "" } else { "s" }
                )));
            }
        }
    }

    while !app.quit {
        app.flush(terminal)?;
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(Ok(event)) = keys.next() => app.on_terminal(event, &tx),
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = tick.tick() => app.frame = app.frame.wrapping_add(1),
        }
    }
    if let Some(r) = app.running.take() {
        r.cancel.cancel();
    }
    app.flush(terminal)?;
    terminal.draw(|frame| frame.render_widget(Paragraph::new(""), frame.area()))?;
    Ok(())
}

impl App {
    // ---- Transcript -----------------------------------------------------------

    fn push(&mut self, lines: Vec<Line<'static>>) {
        self.push_with(lines, vec![Span::raw("  ")]);
    }

    /// Lines whose wrapped continuations should start with `cont` (a gutter, an indent).
    fn push_with(&mut self, lines: Vec<Line<'static>>, cont: Vec<Span<'static>>) {
        if !lines.is_empty() {
            self.out.push((lines, cont));
        }
    }

    /// Write the waiting lines above the live area, wrapped to the terminal's width.
    /// A terminal that will not take them (rare: one that cannot report its
    /// cursor) loses those lines but keeps the session; the error is shown once.
    fn flush(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let width = terminal.size()?.width.max(10);
        for (lines, cont) in std::mem::take(&mut self.out) {
            // Wrapped here, with hanging indents, so continuation lines keep
            // their gutter instead of starting at the left edge.
            let wrapped: Vec<Line<'static>> = lines
                .iter()
                .flat_map(|l| view::wrap_hanging(l, width as usize, &cont))
                .collect();
            let height = wrapped.len().max(1) as u16;
            let para = Paragraph::new(Text::from(wrapped));
            if let Err(e) = terminal.insert_before(height, |buf| para.render(buf.area, buf))
                && !self.flush_failed
            {
                self.flush_failed = true;
                let note = view::error_line(&format!("  (the terminal refused some output: {e})"));
                let _ = terminal.insert_before(1, |buf| {
                    Paragraph::new(Text::from(note)).render(buf.area, buf)
                });
            }
        }
        Ok(())
    }

    /// The unfinished line of the worker's answer goes to the transcript; the block is closed.
    fn flush_text(&mut self) {
        if let Some(r) = &mut self.running {
            let text = std::mem::take(&mut r.text);
            let first = !r.answer_open;
            r.answer_open = false;
            if !text.trim().is_empty() {
                self.out.push((
                    vec![view::answer_line(text.trim_end(), first)],
                    vec![Span::raw("  ")],
                ));
            }
        }
    }
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}
