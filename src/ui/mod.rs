//! The screen: an input box at the bottom of the terminal, the transcript
//! scrolling above it in the terminal's own history (no separate screen, so
//! it stays in your scrollback when you leave), a status line while a task
//! runs, and approvals answered with one key.
//!
//! The live part is a small inline viewport that ratatui redraws; finished
//! pieces (a tool call, the plan, an answer) are pushed above it with
//! `insert_before` and never redrawn again.

mod alert;
pub mod diff;
mod draw;
mod events;
mod input;
mod keys;
mod slash;
#[cfg(test)]
mod tests;
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

use crate::tools::todo::{self, Status, Todo};
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
/// How long the terminal must keep one size before the screen is laid out again.
const SETTLE: Duration = Duration::from_millis(150);
/// Pieces of the transcript kept for laying out again after a resize.
const SHOWN_KEPT: usize = 400;
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
    ("/steps", "the worker's steps of the last plan", false),
    ("/tasks", "this session's tasks and how each ended", false),
    (
        "/report",
        "the last task's result again; /report 2 for the second",
        true,
    ),
    (
        "/todos",
        "the worker's checklist; /todos clear empties it",
        true,
    ),
    ("/memory", "what weft has kept about this project", false),
    ("/cost", "tokens used this session", false),
    ("/mcp", "MCP servers and their tools", false),
    ("/help", "keys and commands", false),
    (
        "/restart",
        "leave and come straight back to this session",
        false,
    ),
    (
        "/handoff",
        "compact, then hand this session to [handoff] command",
        false,
    ),
    ("/quit", "leave", false),
];

/// How much weft asks before acting. Shift-tab cycles through them in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Permission {
    /// Edits, new files and shell commands ask first.
    Ask,
    /// Edits and new files go through; shell commands still ask.
    Edits,
    /// Nothing asks.
    All,
    /// Every task plans first and waits for a word on the plan; approvals ask.
    Plan,
}

impl Permission {
    fn next(self) -> Self {
        match self {
            Self::Ask => Self::Edits,
            Self::Edits => Self::All,
            Self::All => Self::Plan,
            Self::Plan => Self::Ask,
        }
    }

    /// The word in the footer.
    fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Edits => "edits auto",
            Self::All => "all auto",
            Self::Plan => "plan first",
        }
    }

    /// What changed, for the transcript.
    fn note(self) -> &'static str {
        match self {
            Self::Ask => "ask before edits, new files and shell commands",
            Self::Edits => "edits and new files no longer ask; shell commands still do",
            Self::All => "nothing asks",
            Self::Plan => "every task plans first and waits for your word on the plan",
        }
    }
}

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
    /// The tool running now, as its transcript line says it, for the status line.
    doing: Option<String>,
    cancel: CancellationToken,
}

struct App {
    session: Arc<Session>,
    think: bool,
    /// How much weft asks before acting; shift-tab cycles it.
    mode: Permission,
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
    /// Leaving in order to come straight back (`/restart`).
    restart: bool,
    /// `[handoff] command` is set, so `/handoff` has somewhere to go.
    can_handoff: bool,
    /// `/handoff` is compacting first; it leaves once that is done.
    handing_off: bool,
    /// Leaving so that the handoff command takes the session over.
    handoff: bool,
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
    /// Tool calls under way, by id: the name for the result line, the
    /// arguments for the diff under an edit. A helper's calls interleave with
    /// the `delegate` call that started it, so one "last call" is not enough.
    calls: std::collections::HashMap<String, (String, serde_json::Value)>,
    /// How many helpers are at work; their lines are indented under the Delegate line.
    nested: usize,
    /// The last `/resume` listing, so `/resume 2` means something.
    sessions: Vec<Saved>,
    /// The listing was the last thing shown, so a bare number picks from it.
    picking_session: bool,
    /// The worker's checklist as last heard, for the status line and `/todos`.
    todos: Vec<Todo>,
    /// The last plan, whole, for `/steps`.
    plan: Option<String>,
    /// Each finished task's result card this session, for `/tasks` and `/report`.
    reports: Vec<crate::agent::report::Report>,
    /// When the question on screen (an approval, a plan) was asked, and
    /// whether a notification has gone out for it.
    asked_at: Option<(Instant, bool)>,
    /// How the screen gets your attention; the real bell and notification
    /// unless a test listens instead.
    alert: Box<dyn Fn(alert::Alert) + Send + Sync>,
    /// What has been written above the live area lately, kept to be wrapped
    /// again when the terminal changes width.
    shown: std::collections::VecDeque<(Vec<Line<'static>>, Vec<Span<'static>>)>,
    /// When the terminal last changed size; the screen is put right once it
    /// has been still for `SETTLE`.
    resized: Option<Instant>,
    /// The size the screen was last laid out for: the resize events that
    /// queued up while it settled say nothing new.
    laid_out: Option<(u16, u16)>,
}

/// How the screen was left: for good, to be started again on the same
/// session (`/restart`, which picks up a newly installed build), or for the
/// session to be handed to `[handoff] command` (`/handoff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Quit,
    Restart,
    Handoff,
}

pub async fn run(
    session: Arc<Session>,
    yes: bool,
    think: bool,
    resume: Option<Saved>,
    handoff: bool,
) -> Result<Exit> {
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
    let result = event_loop(&mut terminal, session, yes, think, resume, handoff).await;
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
    handoff: bool,
) -> Result<Exit> {
    let mut app = App::new(session, yes, think);
    app.can_handoff = handoff;
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
        // While a window edge is being dragged nothing is drawn: each width
        // would leave its own wrapped copy of the live area behind. Once the
        // size holds, the screen is laid out afresh. Keys wait meanwhile: a
        // pending read holds the terminal's input, and laying out asks the
        // terminal where its cursor is, an answer that comes back as input.
        if let Some(at) = app.resized {
            if at.elapsed() < SETTLE {
                tokio::select! {
                    Some(msg) = rx.recv() => app.on_msg(msg),
                    _ = tokio::time::sleep(SETTLE - at.elapsed()) => {}
                }
                continue;
            }
            app.resized = None;
            app.repaint(terminal)?;
        }
        app.flush(terminal)?;
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            Some(Ok(event)) = keys.next() => app.on_terminal(event, &tx),
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = tick.tick() => app.on_tick(),
        }
    }
    if let Some(r) = app.running.take() {
        r.cancel.cancel();
    }
    app.flush(terminal)?;
    // The live area goes blank and the shell takes over at its top-left,
    // rather than wherever the input box left the cursor.
    let mut top = 0;
    terminal.draw(|frame| {
        top = frame.area().y;
        frame.render_widget(Paragraph::new(""), frame.area());
        frame.set_cursor_position((0, top));
    })?;
    Ok(if app.restart {
        Exit::Restart
    } else if app.handoff {
        Exit::Handoff
    } else {
        Exit::Quit
    })
}

impl App {
    fn new(session: Arc<Session>, yes: bool, think: bool) -> Self {
        Self {
            session,
            think,
            mode: if yes {
                Permission::All
            } else {
                Permission::Ask
            },
            input: Input::default(),
            history: Vec::new(),
            history_at: None,
            menu_at: 0,
            running: None,
            ask: None,
            plan_ask: None,
            quit_armed: false,
            quit: false,
            restart: false,
            can_handoff: false,
            handing_off: false,
            handoff: false,
            frame: 0,
            out: Vec::new(),
            planner_usage: Usage::default(),
            worker_usage: Usage::default(),
            scout_usage: Usage::default(),
            context: 0,
            tasks: 0,
            flush_failed: false,
            calls: std::collections::HashMap::new(),
            nested: 0,
            sessions: Vec::new(),
            picking_session: false,
            todos: Vec::new(),
            plan: None,
            reports: Vec::new(),
            asked_at: None,
            alert: Box::new(alert::send),
            shown: std::collections::VecDeque::new(),
            resized: None,
            laid_out: None,
        }
    }

    // ---- Transcript -----------------------------------------------------------

    fn push(&mut self, lines: Vec<Line<'static>>) {
        self.push_with(lines, vec![Span::raw("  ")]);
    }

    /// Lines whose wrapped continuations should start with `cont` (a gutter, an indent).
    fn push_with(&mut self, lines: Vec<Line<'static>>, cont: Vec<Span<'static>>) {
        if lines.is_empty() {
            return;
        }
        if self.nested == 0 {
            self.out.push((lines, cont));
            return;
        }
        // Inside a helper's run, everything sits two columns further in.
        let pad = "  ".repeat(self.nested);
        let lines = lines
            .into_iter()
            .map(|l| {
                let mut spans = vec![Span::raw(pad.clone())];
                spans.extend(l.spans);
                Line::from(spans)
            })
            .collect();
        let mut indented = vec![Span::raw(pad)];
        indented.extend(cont);
        self.out.push((lines, indented));
    }

    /// After a resize: the terminal has rewrapped (or not) whatever it held,
    /// differently in each terminal, so start clean. Clear the screen, put the
    /// live area at the top, and write the last screenful of the transcript
    /// again at the new width, which brings the live area back to the bottom.
    fn repaint(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let size = terminal.size()?;
        self.laid_out = Some((size.width, size.height));
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        )?;
        terminal.resize(Rect::new(0, 0, size.width, size.height))?;
        // Ratatui draws only what changed since its last frame; the screen it
        // remembers is gone, so the next frame is drawn whole.
        terminal.clear()?;
        let room = size.height.saturating_sub(LIVE_ROWS) as usize;
        let again = recent(&self.shown, size.width as usize, room);
        // Lines still waiting go after the ones shown again.
        let waiting = std::mem::take(&mut self.out);
        self.write(terminal, again)?;
        self.out = waiting;
        Ok(())
    }

    /// Write the waiting lines above the live area, wrapped to the terminal's width.
    /// A terminal that will not take them (rare: one that cannot report its
    /// cursor) loses those lines but keeps the session; the error is shown once.
    fn flush(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        let out = std::mem::take(&mut self.out);
        for block in &out {
            if self.shown.len() == SHOWN_KEPT {
                self.shown.pop_front();
            }
            self.shown.push_back(block.clone());
        }
        self.write(terminal, out)
    }

    fn write(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        out: Vec<(Vec<Line<'static>>, Vec<Span<'static>>)>,
    ) -> Result<()> {
        let width = terminal.size()?.width.max(10);
        for (lines, cont) in out {
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

/// The last `rows` lines of `shown`, wrapped at `width`, oldest first: a
/// screenful, as one piece. A tall piece at the top shows its end.
fn recent(
    shown: &std::collections::VecDeque<(Vec<Line<'static>>, Vec<Span<'static>>)>,
    width: usize,
    rows: usize,
) -> Vec<(Vec<Line<'static>>, Vec<Span<'static>>)> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (block, cont) in shown.iter().rev() {
        let wrapped: Vec<Line<'static>> = block
            .iter()
            .flat_map(|l| view::wrap_hanging(l, width.max(10), cont))
            .collect();
        lines.splice(0..0, wrapped);
        if lines.len() >= rows {
            break;
        }
    }
    let cut = lines.len().saturating_sub(rows);
    if rows == 0 || lines.len() == cut {
        return Vec::new();
    }
    vec![(lines.split_off(cut), Vec::new())]
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}
