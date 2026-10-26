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
    assert!(app.accept_edits);
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
    assert!(!app.accept_edits);
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
