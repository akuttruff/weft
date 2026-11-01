//! Getting your attention: the terminal's bell the moment weft needs an
//! answer, and a macOS notification when the question has waited a while or
//! a long task is done, for when you have gone to another window.

use std::time::Duration;

/// A question waits this long before a notification goes out.
pub const ASK_NOTIFY_AFTER: Duration = Duration::from_secs(30);
/// A task that ran this long says when it is done.
pub const DONE_NOTIFY_AFTER: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// The bell: a sound, or a bounce of the terminal's icon, as it is set up.
    Bell,
    /// A notification in macOS's corner, and the bell with it.
    Notify { title: String, body: String },
}

/// Send an alert for real.
pub fn send(alert: Alert) {
    bell();
    if let Alert::Notify { title, body } = alert {
        notify(title, body);
    }
}

fn bell() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// `osascript` takes the words as arguments, not inside the script, so no
/// quote in them can break it. Waited for on a thread of its own, so a slow
/// notification centre never holds up the screen.
#[cfg(target_os = "macos")]
fn notify(title: String, body: String) {
    use std::process::{Command, Stdio};
    std::thread::spawn(move || {
        let _ = Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                &title,
                &body,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Elsewhere the bell, already rung, has to do.
#[cfg(not(target_os = "macos"))]
fn notify(_title: String, _body: String) {}
