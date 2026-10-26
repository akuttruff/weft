//! The screen as a state machine, driven through the same handlers the
//! terminal uses, with ratatui's test backend for the drawing.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::{mpsc, oneshot};

use super::*;
use crate::config::Config;

fn app() -> (App, mpsc::Sender<UiMsg>, mpsc::Receiver<UiMsg>) {
    let dir = tempfile::tempdir().unwrap();
    let config: Config = toml::from_str(
        r#"
[lm_studio]
[planner]
model = "p"
[worker]
model = "w"
"#,
    )
    .unwrap();
    let session = Arc::new(Session::new(&config, dir.keep()));
    let (tx, rx) = mpsc::channel(16);
    (App::new(session, false, false), tx, rx)
}

fn key(app: &mut App, tx: &mpsc::Sender<UiMsg>, code: KeyCode) {
    app.on_key(KeyEvent::new(code, KeyModifiers::NONE), tx);
}

fn type_text(app: &mut App, tx: &mpsc::Sender<UiMsg>, text: &str) {
    for c in text.chars() {
        key(app, tx, KeyCode::Char(c));
    }
}

/// Every transcript line waiting to be drawn, as plain text.
fn pending(app: &App) -> String {
    app.out
        .iter()
        .flat_map(|(lines, _)| lines.iter())
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_approval_takes_one_key_and_is_noted() {
    let (mut app, tx, _rx) = app();
    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::Ask {
        what: "edit a.rs".into(),
        reply,
    });
    // Ordinary typing is ignored while the question is up.
    key(&mut app, &tx, KeyCode::Char('x'));
    assert!(app.ask.is_some());
    key(&mut app, &tx, KeyCode::Char('a'));
    assert!(app.ask.is_none());
    assert_eq!(answer.blocking_recv().unwrap(), Decision::Always);
    assert!(pending(&app).contains("edit a.rs? always"));
}

#[test]
fn a_plan_goes_on_enter_revises_on_notes_and_stops_on_esc() {
    let (mut app, tx, _rx) = app();

    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(answer.blocking_recv().unwrap(), PlanDecision::Go);

    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    type_text(&mut app, &tx, "skip the tests");
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(
        answer.blocking_recv().unwrap(),
        PlanDecision::Revise("skip the tests".into())
    );
    assert!(app.input.is_empty(), "the notes left the input box");

    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    key(&mut app, &tx, KeyCode::Esc);
    assert_eq!(answer.blocking_recv().unwrap(), PlanDecision::Stop);

    // A slash command at the plan prompt runs as a command; the plan still waits.
    let (reply, mut answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    type_text(&mut app, &tx, "/cost");
    key(&mut app, &tx, KeyCode::Enter);
    assert!(app.plan_ask.is_some(), "the plan is still waiting");
    assert!(pending(&app).contains("0 tasks"), "{}", pending(&app));
    assert!(answer.try_recv().is_err());
    drop(answer);
    app.plan_ask = None;

    // A typed yes or no is an answer, not notes.
    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    type_text(&mut app, &tx, "Yes");
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(answer.blocking_recv().unwrap(), PlanDecision::Go);
    let (reply, answer) = oneshot::channel();
    app.on_msg(UiMsg::AskPlan { reply });
    type_text(&mut app, &tx, "no");
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(answer.blocking_recv().unwrap(), PlanDecision::Stop);
    assert!(pending(&app).contains("plan: revise · skip the tests"));
}

#[test]
fn the_slash_menu_narrows_as_you_type() {
    let (mut app, tx, _rx) = app();
    type_text(&mut app, &tx, "/co");
    assert!(app.menu_open());
    let names: Vec<String> = app.menu_items().into_iter().map(|i| i.name).collect();
    assert_eq!(names, ["/compact", "/cost"]);
    // Tab completes the highlighted one; a command that takes no text runs at once.
    key(&mut app, &tx, KeyCode::Down);
    key(&mut app, &tx, KeyCode::Tab);
    assert!(app.input.is_empty());
    assert!(pending(&app).contains("0 tasks"), "{}", pending(&app));
}

#[test]
fn accept_toggles_and_shows_in_the_footer() {
    let (mut app, tx, _rx) = app();
    type_text(&mut app, &tx, "/accept");
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(app.mode, Permission::Edits);
    assert!(pending(&app).contains("no longer ask"));

    let backend = TestBackend::new(100, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let screen = terminal.backend().buffer().clone();
    let text: String = screen.content().iter().map(|c| c.symbol()).collect();
    assert!(text.contains("What should weft do?"));
    assert!(text.contains("edits auto"));

    type_text(&mut app, &tx, "/accept");
    key(&mut app, &tx, KeyCode::Enter);
    assert_eq!(app.mode, Permission::Ask);
}

#[test]
fn a_bare_number_after_the_resume_listing_is_a_pick() {
    let (mut app, tx, _rx) = app();
    // Nothing to list yet: a number is still a task, and nothing is picked.
    type_text(&mut app, &tx, "/resume");
    key(&mut app, &tx, KeyCode::Enter);
    assert!(!app.picking_session);
    // With a listing up, "1" means the first session; an unknown number says so.
    app.sessions = vec![Saved {
        id: "20261025-120000".into(),
        ..Default::default()
    }];
    app.picking_session = true;
    type_text(&mut app, &tx, "3");
    key(&mut app, &tx, KeyCode::Enter);
    assert!(
        pending(&app).contains("no session 3 in the list"),
        "{}",
        pending(&app)
    );
    assert!(
        app.running.is_none(),
        "it was not sent to the models as a task"
    );
    assert!(
        !app.picking_session,
        "the listing is spent after one answer"
    );
}

#[test]
fn shift_tab_cycles_the_permission_mode() {
    let (mut app, tx, _rx) = app();
    assert_eq!(app.mode, Permission::Ask);
    let mut seen = vec![];
    for _ in 0..4 {
        key(&mut app, &tx, KeyCode::BackTab);
        seen.push(app.mode);
    }
    assert_eq!(
        seen,
        [
            Permission::Edits,
            Permission::All,
            Permission::Plan,
            Permission::Ask
        ]
    );
    assert!(pending(&app).contains("nothing asks"));
}

#[test]
fn readline_keys_edit_the_input() {
    let (mut app, tx, _rx) = app();
    type_text(&mut app, &tx, "fix the tests");
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
    app.on_key(alt('b'), &tx);
    app.on_key(ctrl('k'), &tx);
    assert_eq!(&*app.input, "fix the ");
    app.on_key(ctrl('a'), &tx);
    app.on_key(ctrl('f'), &tx);
    app.on_key(ctrl('d'), &tx);
    assert_eq!(&*app.input, "fx the ");
    app.on_key(ctrl('e'), &tx);
    app.on_key(ctrl('u'), &tx);
    assert!(app.input.is_empty() && !app.quit);
}

#[test]
fn ctrl_c_twice_leaves_and_once_clears_the_input() {
    let (mut app, tx, _rx) = app();
    type_text(&mut app, &tx, "half a thought");
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    app.on_key(ctrl_c, &tx);
    assert!(app.input.is_empty() && !app.quit);
    app.on_key(ctrl_c, &tx);
    assert!(!app.quit, "the first press on an empty box only arms");
    app.on_key(ctrl_c, &tx);
    assert!(app.quit);
}

#[test]
fn a_helpers_lines_sit_under_the_delegate_line() {
    use serde_json::json;
    let (mut app, _tx, _rx) = app();
    let start = |id: &str, name: &str, summary: &str| {
        UiMsg::Agent(AgentEvent::ToolStart {
            id: id.into(),
            name: name.into(),
            summary: summary.into(),
            args: json!({}),
        })
    };
    let done = |id: &str, output: &str| {
        UiMsg::Agent(AgentEvent::ToolDone {
            id: id.into(),
            ok: true,
            output: output.into(),
        })
    };
    app.on_msg(start("d1", "delegate", "Delegate write the tests"));
    app.on_msg(start("r1", "read_file", "Read a.rs"));
    app.on_msg(done("r1", "1\tfn main() {}"));
    app.on_msg(done("d1", "Wrote the tests."));
    app.on_msg(start("b1", "bash", "Bash npx vitest run"));
    let text = pending(&app);
    assert!(text.starts_with("⏺ Delegate write the tests"), "{text}");
    assert!(
        text.contains("\n  ⏺ Read a.rs"),
        "the helper's call is indented: {text}"
    );
    assert!(
        text.contains("\n    ⎿ Read 1 line"),
        "and so is its result: {text}"
    );
    assert!(
        text.contains("\n  ⎿ Wrote the tests."),
        "the delegate's own result is not: {text}"
    );
    assert!(
        text.contains("\n⏺ Bash npx vitest run"),
        "back at the margin after: {text}"
    );
}

#[test]
fn a_dropped_file_path_is_a_task_not_a_command() {
    assert_eq!(slash::command_of("/cost"), Some(("/cost", "")));
    assert_eq!(
        slash::command_of("/plan  fix it "),
        Some(("/plan", "fix it"))
    );
    assert_eq!(slash::command_of("/my-cmd x"), Some(("/my-cmd", "x")));
    assert_eq!(slash::command_of("/Users/a/Shot.png what is this"), None);
    assert_eq!(slash::command_of("/var/folders/x/y.png"), None);
    assert_eq!(slash::command_of("/"), None);
    assert_eq!(
        slash::unescape_dropped("/tmp/Screenshot\\ 2026-10-26\\ at\\ 12.09.09\\ AM.png why"),
        "/tmp/Screenshot 2026-10-26 at 12.09.09 AM.png why"
    );
    // No menu pops up for a path, so typing one is not hijacked.
    let (mut app, tx, _rx) = app();
    type_text(&mut app, &tx, "/Users/a");
    assert!(!app.menu_open());
}
