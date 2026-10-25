//! The screen: an input box at the bottom of the terminal, the transcript
//! scrolling above it in the terminal's own history (no separate screen, so
//! it stays in your scrollback when you leave), a status line while a task
//! runs, and approvals answered with one key.
//!
//! The live part is a small inline viewport that ratatui redraws; finished
//! pieces (a tool call, the plan, an answer) are pushed above it with
//! `insert_before` and never redrawn again.

pub mod diff;
mod input;
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
    ("/clear", "start a fresh conversation", false),
    ("/compact", "fold earlier tasks into a note", false),
    ("/undo", "put back the files the last task changed", false),
    ("/resume", "pick up an earlier session", true),
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

    // ---- Messages from the agent ----------------------------------------------

    fn on_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Ask { what, reply } => self.ask = Some((what, reply)),
            UiMsg::AskPlan { reply } => {
                self.flush_text();
                self.plan_ask = Some(reply);
            }
            UiMsg::Done(result) => self.on_done(result),
            UiMsg::Compacted(result) => {
                self.running = None;
                match result {
                    Ok(done) => self.on_agent(done.event()),
                    Err(WorkerError::Cancelled) => self.push(view::error_line("  ⎿ Interrupted")),
                    Err(e) => self.push(view::error_line(&format!("  ⎿ could not compact: {e:#}"))),
                }
            }
            UiMsg::Agent(evt) => self.on_agent(evt),
        }
    }

    fn on_agent(&mut self, evt: AgentEvent) {
        match evt {
            AgentEvent::Phase { name } => {
                self.flush_text();
                let name: &'static str = match name.as_str() {
                    "planning" => "planning",
                    "working" => "working",
                    "reviewing" => "reviewing",
                    "fixing" => "fixing",
                    "compacting" => "compacting",
                    _ => "working",
                };
                if let Some(r) = &mut self.running {
                    r.phase = name;
                }
                self.push(view::phase_line(name));
            }
            AgentEvent::TurnStart { role } => {
                if let Some(r) = &mut self.running {
                    r.turn = Some((role, 0));
                }
            }
            AgentEvent::Reasoning { text, .. } => {
                if let Some(Running {
                    turn: Some((_, n)), ..
                }) = &mut self.running
                {
                    *n += text.len() / 4;
                }
            }
            AgentEvent::Text {
                role: Role::Worker,
                text,
            } => {
                if let Some(r) = &mut self.running {
                    r.text.push_str(&text);
                    // Whole lines go straight to the transcript; the partial last line stays live.
                    while let Some(at) = r.text.find('\n') {
                        let line: String = r.text.drain(..=at).collect();
                        let line = line.trim_end_matches('\n');
                        if line.trim().is_empty() && !r.answer_open {
                            continue;
                        }
                        let first = !r.answer_open;
                        r.answer_open = true;
                        self.out
                            .push((vec![view::answer_line(line, first)], vec![Span::raw("  ")]));
                    }
                }
            }
            AgentEvent::Text { .. } => {}
            AgentEvent::ToolStart {
                summary,
                name,
                args,
                ..
            } => {
                self.last_tool = (name, args);
                self.flush_text();
                self.push(view::tool_line(&summary));
            }
            AgentEvent::ToolDone { ok, output, .. } => {
                let (tool, args) = &self.last_tool;
                let line = view::result_line(ok, &output, tool);
                // An edit or a new file is shown as the code it changed.
                let shown = if ok {
                    diff::rows(tool, args, &output)
                } else {
                    None
                };
                self.push_with(line, vec![Span::raw("    ")]);
                if let Some((rows, hidden)) = shown {
                    self.push_with(
                        view::diff_lines(&rows, hidden),
                        vec![Span::raw(view::DIFF_INDENT)],
                    );
                }
            }
            AgentEvent::Plan { text } => self.push_with(
                view::plan_lines(&text),
                vec![Span::styled("  │ ", Style::new().fg(Color::Cyan))],
            ),
            AgentEvent::Review { approved, text } => self.push(view::review_lines(approved, &text)),
            AgentEvent::Hook { ok, text } => {
                self.flush_text();
                if text.is_empty() {
                    return;
                }
                if ok {
                    self.push(view::note_line(&format!("  {text}")));
                } else {
                    self.push(view::error_line(&format!("  {text}")));
                }
            }
            AgentEvent::Trimmed { dropped, tokens } => {
                self.flush_text();
                self.push(view::note_line(&format!(
                    "  dropped {dropped} earlier tool result{} to make room · context {}",
                    if dropped == 1 { "" } else { "s" },
                    view::k(tokens),
                )));
            }
            AgentEvent::Compacted {
                before,
                after,
                tasks,
            } => {
                self.context = after;
                self.push(view::note_line(&format!(
                    "  compacted: {tasks} earlier task{} folded into a note · context {} → ~{} tokens",
                    if tasks == 1 { "" } else { "s" },
                    view::k(before),
                    view::k(after),
                )));
            }
            AgentEvent::TurnEnd { role, usage, .. } => {
                match role {
                    Role::Planner => self.planner_usage = add(self.planner_usage, usage),
                    Role::Scout => self.scout_usage = add(self.scout_usage, usage),
                    Role::Worker => {
                        self.worker_usage = add(self.worker_usage, usage);
                        self.context = usage.prompt_tokens + usage.completion_tokens;
                    }
                }
                if role == Role::Worker {
                    self.flush_text();
                }
                if let Some(r) = &mut self.running {
                    r.turn = None;
                }
            }
        }
    }

    fn on_done(&mut self, result: Result<TandemOutcome, WorkerError>) {
        self.flush_text();
        let began = self.running.take().map(|r| r.began);
        let secs = began.map(|b| b.elapsed().as_secs()).unwrap_or(0);
        self.ask = None;
        match result {
            Ok(out) => {
                let review = match (out.reviews.last(), out.fixes_pending) {
                    (None, _) => "",
                    (Some(_), true) => " · fixes still pending",
                    (Some((true, _)), false) => " · reviewed",
                    (Some((false, _)), false) => " · not approved",
                };
                let touched = if out.touched.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", out.touched.join(", "))
                };
                self.push(view::note_line(&format!(
                    "  {} · planner {} · worker {} tokens{touched}{review}",
                    view::clock(secs),
                    view::k(view::total(out.planner_usage)),
                    view::k(view::total(out.worker_usage)),
                )));
            }
            Err(WorkerError::Cancelled) => self.push(view::error_line("  ⎿ Interrupted")),
            Err(e) => self.push(view::error_line(&format!("  ⎿ {e:#}"))),
        }
    }

    // ---- Keys -----------------------------------------------------------------

    fn on_terminal(&mut self, event: Event, tx: &mpsc::Sender<UiMsg>) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key, tx),
            Event::Paste(text) => {
                self.input
                    .insert_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.history_at = None;
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent, tx: &mpsc::Sender<UiMsg>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A plan waiting for a word: enter goes, esc stops, typed notes + enter
        // send it back. Everything else edits the notes as usual.
        if self.plan_ask.is_some() {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            match (key.code, ctrl, alt) {
                (KeyCode::Enter, false, false) => {
                    let notes = self.input.trim().to_string();
                    self.input.clear();
                    self.answer_plan(if notes.is_empty() {
                        PlanDecision::Go
                    } else {
                        PlanDecision::Revise(notes)
                    });
                    return;
                }
                (KeyCode::Esc, _, _) | (KeyCode::Char('c'), true, _) => {
                    self.input.clear();
                    self.answer_plan(PlanDecision::Stop);
                    return;
                }
                _ => {}
            }
        }

        // An approval question takes the keyboard until it is answered.
        if let Some((_, _)) = &self.ask {
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => Some(Decision::Once),
                KeyCode::Char('a') | KeyCode::Char('A') => Some(Decision::Always),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Decision::No),
                KeyCode::Char('c') if ctrl => Some(Decision::No),
                _ => None,
            };
            if let Some(d) = decision
                && let Some((what, reply)) = self.ask.take()
            {
                let word = match d {
                    Decision::Once => "yes",
                    Decision::Always => "always",
                    Decision::No => "no",
                };
                self.push(view::note_line(&format!("  {what}? {word}")));
                let _ = reply.send(d);
                if d == Decision::No && key.code == KeyCode::Char('c') && ctrl {
                    self.stop();
                }
            }
            return;
        }

        if key.code != KeyCode::Char('c') || !ctrl {
            self.quit_armed = false;
        }
        match (key.code, ctrl) {
            (KeyCode::Char('c'), true) => {
                if self.running.is_some() {
                    self.stop();
                } else if !self.input.is_empty() {
                    self.input.clear();
                } else if self.quit_armed {
                    self.quit = true;
                } else {
                    self.quit_armed = true;
                }
            }
            (KeyCode::Char('d'), true) if self.input.is_empty() => self.quit = true,
            (KeyCode::Esc, _) => {
                if self.running.is_some() {
                    self.stop();
                } else {
                    self.input.clear();
                    self.history_at = None;
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Char('j'), true)
                if ctrl || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert('\n');
            }
            (KeyCode::Enter, _) => {
                if let Some(stripped) = self.input.strip_suffix('\\') {
                    self.input.set(format!("{stripped}\n"));
                } else if self.menu_open() {
                    self.complete_menu(tx);
                } else {
                    self.submit(tx);
                }
            }
            (KeyCode::Tab, _) if self.menu_open() => self.complete_menu(tx),
            (KeyCode::Backspace, _) => {
                self.input.backspace();
                self.history_at = None;
            }
            (KeyCode::Char('u'), true) => self.input.clear(),
            (KeyCode::Char('w'), true) => self.input.delete_word(),
            (KeyCode::Left, _) => self.input.left(),
            (KeyCode::Right, _) => self.input.right(),
            (KeyCode::Home, _) | (KeyCode::Char('a'), true) => self.input.home(),
            (KeyCode::End, _) | (KeyCode::Char('e'), true) => self.input.end(),
            (KeyCode::Delete, _) => self.input.delete(),
            (KeyCode::Up, _) => {
                if self.menu_open() {
                    self.menu_at = self.menu_at.saturating_sub(1);
                } else if !self.input.contains('\n') {
                    self.recall(-1);
                }
            }
            (KeyCode::Down, _) => {
                if self.menu_open() {
                    self.menu_at =
                        (self.menu_at + 1).min(self.menu_items().len().saturating_sub(1));
                } else if !self.input.contains('\n') {
                    self.recall(1);
                }
            }
            (KeyCode::Char(c), false) => {
                self.input.insert(c);
                self.history_at = None;
                self.menu_at = 0;
            }
            _ => {}
        }
    }

    fn stop(&mut self) {
        if let Some(r) = &self.running {
            r.cancel.cancel();
        }
    }

    fn answer_plan(&mut self, decision: PlanDecision) {
        let Some(reply) = self.plan_ask.take() else {
            return;
        };
        let note = match &decision {
            PlanDecision::Go => "  plan: go".to_string(),
            PlanDecision::Stop => "  plan: stop".to_string(),
            PlanDecision::Revise(notes) => format!("  plan: revise · {notes}"),
        };
        self.push(view::note_line(&note));
        let _ = reply.send(decision);
    }

    /// Walk the history with the arrow keys.
    fn recall(&mut self, step: i32) {
        if self.history.is_empty() {
            return;
        }
        let at = match (self.history_at, step) {
            (None, s) if s < 0 => Some(self.history.len() - 1),
            (None, _) => None,
            (Some(0), s) if s < 0 => Some(0),
            (Some(i), s) if s < 0 => Some(i - 1),
            (Some(i), _) if i + 1 >= self.history.len() => None,
            (Some(i), _) => Some(i + 1),
        };
        self.history_at = at;
        self.input
            .set(at.map(|i| self.history[i].clone()).unwrap_or_default());
    }

    // ---- The slash menu ---------------------------------------------------------

    fn menu_open(&self) -> bool {
        self.input.starts_with('/')
            && !self.input.contains(char::is_whitespace)
            && !self.menu_items().is_empty()
    }

    /// Built-in commands, then the custom ones from `.weft/commands`, that
    /// start with what has been typed.
    fn menu_items(&self) -> Vec<MenuItem> {
        let typed = &*self.input;
        let mut items: Vec<MenuItem> = COMMANDS
            .iter()
            .filter(|(name, _, _)| name.starts_with(typed))
            .map(|(name, what, takes_task)| MenuItem {
                name: name.to_string(),
                what: what.to_string(),
                takes_task: *takes_task,
            })
            .collect();
        for c in commands::load(&self.session.ctx.root) {
            let name = format!("/{}", c.name);
            if name.starts_with(typed) {
                items.push(MenuItem {
                    name,
                    what: c.description.clone(),
                    takes_task: c.body.contains("$ARGUMENTS"),
                });
            }
        }
        items
    }

    fn complete_menu(&mut self, tx: &mpsc::Sender<UiMsg>) {
        let items = self.menu_items();
        let Some(item) = items.get(self.menu_at.min(items.len().saturating_sub(1))) else {
            return;
        };
        if item.takes_task {
            self.input.set(format!("{} ", item.name));
        } else {
            self.input.set(item.name.clone());
            self.submit(tx);
        }
    }

    // ---- Submitting -------------------------------------------------------------

    fn submit(&mut self, tx: &mpsc::Sender<UiMsg>) {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return;
        }
        self.input.clear();
        self.history_at = None;
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        let (command, rest) = match line.split_once(char::is_whitespace) {
            Some((c, r)) if c.starts_with('/') => (c, r.trim().to_string()),
            _ => (line.as_str(), String::new()),
        };
        match command {
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/think" => {
                self.think = !self.think;
                self.push(view::note_line(&format!(
                    "thinking {}",
                    if self.think { "on" } else { "off" }
                )));
            }
            "/clear" => {
                self.session.clear();
                self.push(view::note_line("fresh conversation"));
            }
            "/compact" => self.compact(tx),
            "/undo" => {
                if self.running.is_some() {
                    self.push(view::note_line("a task is running; esc stops it"));
                } else {
                    match self.session.undo() {
                        Ok(Some(files)) => self.push(view::note_line(&format!(
                            "put back {}",
                            files.join(", ")
                        ))),
                        Ok(None) => self.push(view::note_line("nothing to undo")),
                        Err(e) => self.push(view::error_line(&format!("  undo failed: {e:#}"))),
                    }
                }
            }
            "/resume" if rest.is_empty() => {
                let me = self.session.transcript_id();
                match self.sessions_dir().and_then(|d| transcript::list(&d)) {
                    Ok(all) => {
                        self.sessions = all.into_iter().filter(|s| Some(&s.id) != me.as_ref()).collect();
                        let lines = view::session_lines(&self.sessions);
                        self.push(lines);
                    }
                    Err(e) => self.push(view::error_line(&format!("  {e:#}"))),
                }
            }
            "/resume" => {
                // A number picks from the last listing; anything else is an id.
                let picked = match rest.parse::<usize>() {
                    Ok(n) => self
                        .sessions
                        .get(n.wrapping_sub(1))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no session {n} in the list; /resume shows it")),
                    Err(_) => self.sessions_dir().and_then(|d| transcript::find(&d, &rest)),
                };
                match picked {
                    Ok(saved) => self.resume(saved),
                    Err(e) => self.push(view::error_line(&format!("  {e:#}"))),
                }
            }
            "/cost" => self.push(view::note_line(&format!(
                "{} tasks · planner {} tokens ({} thinking) · worker {} tokens · scout {} tokens · context {} of {}",
                self.tasks,
                view::k(view::total(self.planner_usage)),
                view::k(self.planner_usage.reasoning_tokens),
                view::k(view::total(self.worker_usage)),
                view::k(view::total(self.scout_usage)),
                view::k(self.context),
                view::k(self.session.window),
            ))),
            "/mcp" => {
                if self.session.mcp.is_empty() {
                    self.push(view::note_line(&format!(
                        "no MCP servers; list them in {}",
                        crate::mcp::McpConfig::path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "~/.config/weft/mcp.json".into())
                    )));
                } else {
                    self.push(view::mcp_lines(&self.session.mcp));
                }
            }
            "/help" => {
                let mut lines = Vec::new();
                for (name, what, _) in COMMANDS {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {name:<10}"), Style::new().fg(Color::Cyan)),
                        Span::styled(what.to_string(), Style::new().dim()),
                    ]));
                }
                for c in commands::load(&self.session.ctx.root) {
                    lines.push(Line::from(vec![
                        Span::styled(format!("  /{:<9}", c.name), Style::new().fg(Color::Cyan)),
                        Span::styled(
                            format!("{} · {}", c.description, c.from.display()),
                            Style::new().dim(),
                        ),
                    ]));
                }
                lines.push(Line::from(Span::styled("  enter sends · ctrl-j or \\ + enter for a newline · ← → home end move · ↑↓ history · esc stops a task · y/a/n answer approvals · ctrl-c twice leaves", Style::new().dim())));
                self.push(lines);
            }
            "/plan" | "/go" if rest.is_empty() => {
                self.push(view::note_line(&format!("usage: {command} <task>")))
            }
            "/plan" => self.start(rest, Mode::Plan, tx),
            "/go" => self.start(rest, Mode::Go, tx),
            c if c.starts_with('/') => {
                // A custom command: the file is the task, shown as what was typed.
                let custom = commands::load(&self.session.ctx.root)
                    .into_iter()
                    .find(|cmd| format!("/{}", cmd.name) == c);
                match custom {
                    Some(cmd) => {
                        let task = commands::expand(&cmd.body, &rest);
                        self.start_shown(line.clone(), task, cmd.mode, tx);
                    }
                    None => self.push(view::note_line(&format!(
                        "unknown command {c}; /help lists them"
                    ))),
                }
            }
            _ => self.start(line, Mode::Auto, tx),
        }
    }

    /// Fold the conversation now, on its own tokio task like a task.
    fn compact(&mut self, tx: &mpsc::Sender<UiMsg>) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        if self.session.depth() == 0 {
            self.push(view::note_line("nothing to compact yet"));
            return;
        }
        let cancel = CancellationToken::new();
        self.running = Some(Running {
            began: Instant::now(),
            phase: "compacting",
            turn: None,
            text: String::new(),
            answer_open: false,
            cancel: cancel.clone(),
        });
        let session = self.session.clone();
        let before = self.context;
        let tx = tx.clone();
        tokio::spawn(async move {
            let (events, mut agent_rx) = mpsc::channel::<AgentEvent>(64);
            let forward = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(evt) = agent_rx.recv().await {
                        if tx.send(UiMsg::Agent(evt)).await.is_err() {
                            break;
                        }
                    }
                })
            };
            let result = session.compact(events, cancel, before).await;
            let _ = forward.await;
            let _ = tx.send(UiMsg::Compacted(result)).await;
        });
    }

    fn sessions_dir(&self) -> Result<std::path::PathBuf> {
        transcript::dir_for(&self.session.ctx.root)
    }

    /// Make an earlier session the current one and show what it holds.
    fn resume(&mut self, saved: Saved) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        if let Err(e) = self.session.resume(&saved) {
            self.push(view::error_line(&format!("  could not resume: {e:#}")));
            return;
        }
        self.planner_usage = saved.planner_usage;
        self.worker_usage = saved.worker_usage;
        self.scout_usage = saved.scout_usage;
        self.context = saved.context;
        self.tasks = saved.tasks.len();
        self.push(view::replay_lines(&saved));
    }

    /// Hand a task to the session on its own tokio task; its events come back through `tx`.
    fn start(&mut self, task: String, mode: Mode, tx: &mpsc::Sender<UiMsg>) {
        self.start_shown(task.clone(), task, mode, tx);
    }

    /// The same, showing `shown` in the transcript rather than the task text.
    fn start_shown(&mut self, shown: String, task: String, mode: Mode, tx: &mpsc::Sender<UiMsg>) {
        if self.running.is_some() {
            self.push(view::note_line("a task is running; esc stops it"));
            return;
        }
        self.tasks += 1;
        self.push(view::user_line(&shown));
        let cancel = CancellationToken::new();
        self.running = Some(Running {
            began: Instant::now(),
            phase: "starting",
            turn: None,
            text: String::new(),
            answer_open: false,
            cancel: cancel.clone(),
        });

        let session = self.session.clone();
        let think = self.think;
        let yes = self.yes;
        let tx = tx.clone();
        tokio::spawn(async move {
            let (events, mut agent_rx) = mpsc::channel::<AgentEvent>(256);
            let forward = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(evt) = agent_rx.recv().await {
                        if tx.send(UiMsg::Agent(evt)).await.is_err() {
                            break;
                        }
                    }
                })
            };
            let approver: Box<dyn Approver> = if yes {
                Box::new(Yes)
            } else {
                Box::new(UiApprover { tx: tx.clone() })
            };
            let result = session
                .run_task(&task, mode, think, approver.as_ref(), events, cancel)
                .await;
            let _ = forward.await;
            let _ = tx.send(UiMsg::Done(result)).await;
        });
    }

    // ---- Drawing ----------------------------------------------------------------

    fn render(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        if self.menu_open() {
            self.render_menu(frame, area);
            return;
        }
        let [tail, status, input, footer] = Layout::vertical([
            Constraint::Length(TAIL_ROWS),
            Constraint::Length(1),
            Constraint::Length(BOX_ROWS),
            Constraint::Length(1),
        ])
        .areas(area);

        // The last few lines of the answer as it streams.
        if let Some(r) = &self.running
            && !r.text.is_empty()
        {
            let para = Paragraph::new(Text::from(vec![view::answer_line(&r.text, !r.answer_open)]))
                .wrap(Wrap { trim: false });
            let count = para.line_count(tail.width) as u16;
            frame.render_widget(para.scroll((count.saturating_sub(tail.height), 0)), tail);
        }

        frame.render_widget(Paragraph::new(self.status_line()), status);

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().dim());
        let inner = block.inner(input);
        frame.render_widget(block, input);
        let shown = if self.input.is_empty() {
            Text::from(Line::from(vec![
                Span::styled("> ", Style::new().bold()),
                Span::styled("What should weft do?", Style::new().dim()),
            ]))
        } else {
            let mut lines = Vec::new();
            for (i, l) in self.input.split('\n').enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(if i == 0 { "> " } else { "  " }, Style::new().bold()),
                    Span::raw(l.to_string()),
                ]));
            }
            Text::from(lines)
        };
        let (row, col) = self.input.cursor_at(inner.width);
        let scroll = row.saturating_sub(inner.height.saturating_sub(1));
        frame.render_widget(
            Paragraph::new(shown)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            inner,
        );
        frame.set_cursor_position((inner.x + col, inner.y + row - scroll));

        frame.render_widget(Paragraph::new(self.footer_line()), footer);
    }

    /// While a command is being typed, the live area shows the line and the matching commands.
    fn render_menu(&self, frame: &mut ratatui::Frame, area: Rect) {
        let items = self.menu_items();
        let mut lines = vec![Line::from(vec![
            Span::styled("> ", Style::new().bold()),
            Span::raw(self.input.to_string()),
        ])];
        for (i, item) in items.iter().enumerate() {
            let style = if i == self.menu_at.min(items.len() - 1) {
                Style::new().bold().fg(Color::Cyan)
            } else {
                Style::new()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", item.name), style),
                Span::styled(item.what.clone(), Style::new().dim()),
            ]));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), area);
        frame.set_cursor_position((area.x + 2 + self.input.width() as u16, area.y));
    }

    fn status_line(&self) -> Line<'static> {
        if self.plan_ask.is_some() {
            return Line::from(vec![
                Span::styled("? ", Style::new().fg(Color::Yellow).bold()),
                Span::styled("Follow this plan?  ", Style::new().fg(Color::Yellow)),
                Span::styled(
                    "enter to go · type notes + enter to revise · esc to stop",
                    Style::new().dim(),
                ),
            ]);
        }
        if let Some((what, _)) = &self.ask {
            return Line::from(vec![
                Span::styled("? ", Style::new().fg(Color::Yellow).bold()),
                Span::styled(
                    format!("Allow weft to {what}?  "),
                    Style::new().fg(Color::Yellow),
                ),
                Span::styled("(y) once  (a) always  (n) no", Style::new().dim()),
            ]);
        }
        if let Some(r) = &self.running {
            let verb = match r.phase {
                "planning" => "Planning",
                "working" => "Working",
                "reviewing" => "Reviewing",
                "fixing" => "Fixing",
                "compacting" => "Compacting",
                _ => "Thinking about it",
            };
            let mut s = format!("{verb}… {}", view::clock(r.began.elapsed().as_secs()));
            if let Some((Role::Planner, n)) = r.turn
                && n > 0
            {
                s.push_str(&format!(" · ~{} thinking tokens", view::k(n as u32)));
            }
            return Line::from(vec![
                Span::styled(
                    format!("{} ", SPINNER[self.frame % SPINNER.len()]),
                    Style::new().fg(Color::Magenta),
                ),
                Span::styled(s, Style::new().fg(Color::Magenta)),
                Span::styled("  esc to interrupt", Style::new().dim()),
            ]);
        }
        if self.quit_armed {
            return Line::from(Span::styled(
                "press ctrl-c again to leave",
                Style::new().dim(),
            ));
        }
        Line::from("")
    }

    fn footer_line(&self) -> Line<'static> {
        let pct = (self.context as f32 / self.session.window.max(1) as f32 * 100.0).round() as u32;
        // Name the model that has the floor: the planner while it plans or
        // reviews, the worker otherwise.
        let (model, doing) = match &self.running {
            Some(Running {
                turn: Some((Role::Planner, _)),
                phase,
                ..
            }) => (self.session.planner_name(), *phase),
            Some(Running {
                turn: Some((Role::Scout, _)),
                phase,
                ..
            }) => (
                self.session.scout_name(),
                if *phase == "compacting" {
                    "compacting"
                } else {
                    "scouting"
                },
            ),
            Some(r) => (self.session.worker_name(), r.phase),
            None => (self.session.worker_name(), "idle"),
        };
        Line::from(Span::styled(
            format!(
                "  {model} · {doing} · think {} · context {pct}% · {} in conversation · / for commands",
                if self.think { "on" } else { "off" },
                self.session.depth()
            ),
            Style::new().dim(),
        ))
    }
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}
