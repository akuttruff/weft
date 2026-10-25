//! The transcript: everything a session did, one JSON line per record, so a
//! run can be read back later and a session picked up where it stopped.
//!
//! Files live outside the project, in
//! `~/.local/share/weft/sessions/<folder slug>/<id>.jsonl`, so they never
//! show up in `git status` and the worker never reads its own history by
//! accident. The slug is the folder's path with `/` turned into `-`.
//!
//! A file holds the worker's conversation (every message as it was appended),
//! the screen events of each task, and a few markers: the task text, how it
//! ended, `/clear`, a resume. Reading one back gives the conversation as it
//! stood at the end, which is what resuming needs.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use jiff::Zoned;
use serde::{Deserialize, Serialize};

use crate::agent::{AgentEvent, Role};
use crate::lm::{Message, Usage};

/// One line of the file.
// Rust: `tag = "kind"` makes serde write the variant's name into a `kind`
// field beside the variant's own fields, so each line reads as
// `{"kind":"task","at":"…","text":"…"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// The first line: which session this is.
    Meta {
        id: String,
        root: String,
        started: String,
        planner: String,
        worker: String,
        scout: String,
    },
    /// A task was given.
    Task { at: String, text: String },
    /// A message was appended to the worker's conversation.
    Message { message: Message },
    /// Something the screen was told (the streamed text aside).
    Event { at: String, event: AgentEvent },
    /// The task ended, well or not.
    Done {
        at: String,
        seconds: f32,
        error: Option<String>,
    },
    /// `/clear`: the conversation starts over.
    Clear { at: String },
    /// The conversation was replaced wholesale, as compaction does: `note`
    /// says why, `messages` is the new conversation, system prompt aside.
    Replace {
        at: String,
        note: String,
        messages: Vec<Message>,
    },
    /// The session was reopened.
    Resumed { at: String },
}

/// The writer: an open session file.
pub struct Transcript {
    pub id: String,
    pub path: PathBuf,
    file: Mutex<File>,
}

/// Where a folder's sessions live.
pub fn dir_for(root: &Path) -> Result<PathBuf> {
    let home = std::env::home_dir().context("no home folder")?;
    Ok(home.join(".local/share/weft/sessions").join(slug(root)))
}

/// `/Users/amie/Code/weft` → `-Users-amie-Code-weft`.
pub fn slug(root: &Path) -> String {
    let s = root.to_string_lossy().replace(['/', '\\'], "-");
    if s.is_empty() { "-".to_string() } else { s }
}

/// The local time, to the second, as `2026-10-25T19:03:11-07:00`.
pub fn now() -> String {
    Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

impl Transcript {
    /// A new session file in `dir`, named after the moment it starts.
    pub fn start(dir: &Path, meta: Record) -> Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let stamp = Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
        // Two sessions in the same second get distinct names.
        let mut id = stamp.clone();
        let mut n = 1;
        while dir.join(format!("{id}.jsonl")).exists() {
            n += 1;
            id = format!("{stamp}-{n}");
        }
        let path = dir.join(format!("{id}.jsonl"));
        let meta = match meta {
            Record::Meta {
                root,
                started,
                planner,
                worker,
                scout,
                ..
            } => Record::Meta {
                id: id.clone(),
                root,
                started,
                planner,
                worker,
                scout,
            },
            other => {
                return Err(anyhow!(
                    "a transcript starts with its meta line, not {other:?}"
                ));
            }
        };
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let t = Self {
            id,
            path,
            file: Mutex::new(file),
        };
        t.write(&meta)?;
        Ok(t)
    }

    /// Reopen an existing file to append to it.
    pub fn open(path: &Path) -> Result<Self> {
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .context("a session file has a name")?;
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self {
            id,
            path: path.to_path_buf(),
            file: Mutex::new(file),
        })
    }

    pub fn write(&self, record: &Record) -> Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

/// A session read back from its file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    pub id: String,
    pub path: PathBuf,
    pub started: String,
    /// When the last record was written.
    pub last: String,
    /// Every task given, in order, `/clear` or not.
    pub tasks: Vec<String>,
    /// The worker's conversation as it stood at the end, system prompt aside.
    pub messages: Vec<Message>,
    pub planner_usage: Usage,
    pub worker_usage: Usage,
    pub scout_usage: Usage,
    /// The worker's last prompt size.
    pub context: u32,
}

/// Read one session file.
pub fn read(path: &Path) -> Result<Saved> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut saved = Saved {
        path: path.to_path_buf(),
        ..Default::default()
    };
    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(&line)
            .with_context(|| format!("{} line {}", path.display(), n + 1))?;
        saved.take(record);
    }
    if saved.id.is_empty() {
        return Err(anyhow!("{} has no meta line", path.display()));
    }
    Ok(saved)
}

impl Saved {
    fn take(&mut self, record: Record) {
        match record {
            Record::Meta { id, started, .. } => {
                self.id = id;
                self.last = started.clone();
                self.started = started;
            }
            Record::Task { at, text } => {
                self.last = at;
                self.tasks.push(text);
            }
            Record::Message { message } => self.messages.push(message),
            Record::Event { at, event } => {
                self.last = at;
                if let AgentEvent::TurnEnd { role, usage, .. } = event {
                    match role {
                        Role::Planner => self.planner_usage = add(self.planner_usage, usage),
                        Role::Scout => self.scout_usage = add(self.scout_usage, usage),
                        Role::Worker => {
                            self.worker_usage = add(self.worker_usage, usage);
                            self.context = usage.prompt_tokens + usage.completion_tokens;
                        }
                    }
                }
            }
            Record::Done { at, .. } | Record::Resumed { at } => self.last = at,
            Record::Clear { at } => {
                self.last = at;
                self.messages.clear();
                self.context = 0;
            }
            Record::Replace { at, messages, .. } => {
                self.last = at;
                self.messages = messages;
            }
        }
    }

    /// The tasks and the answers they got, from the conversation as it stands.
    pub fn exchanges(&self) -> Vec<(String, String)> {
        crate::agent::tandem::exchanges(&self.messages)
    }
}

/// Every session of a folder, newest first.
pub fn list(dir: &Path) -> Result<Vec<Saved>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "jsonl")
            && let Ok(saved) = read(&path)
        {
            out.push(saved);
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// How many sessions a folder has, without reading them.
pub fn count(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .count()
        })
        .unwrap_or(0)
}

/// The session `which` names: an id, or `last` for the newest.
pub fn find(dir: &Path, which: &str) -> Result<Saved> {
    if which == "last" {
        return list(dir)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no earlier session in this folder"));
    }
    let path = dir.join(format!("{which}.jsonl"));
    if !path.exists() {
        return Err(anyhow!(
            "no session {which} in this folder; `weft sessions` lists them"
        ));
    }
    read(&path)
}

fn add(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> Record {
        Record::Meta {
            id: String::new(),
            root: "/tmp/p".into(),
            started: now(),
            planner: "p".into(),
            worker: "w".into(),
            scout: "s".into(),
        }
    }

    #[test]
    fn writes_and_reads_a_session_back() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transcript::start(dir.path(), meta()).unwrap();
        t.write(&Record::Task {
            at: now(),
            text: "add a test".into(),
        })
        .unwrap();
        t.write(&Record::Message {
            message: Message::User("add a test".into()),
        })
        .unwrap();
        t.write(&Record::Event {
            at: now(),
            event: AgentEvent::TurnEnd {
                role: Role::Worker,
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    reasoning_tokens: 0,
                },
                seconds: 1.5,
            },
        })
        .unwrap();
        t.write(&Record::Message {
            message: Message::Assistant {
                content: "done".into(),
                reasoning: None,
                tool_calls: vec![],
            },
        })
        .unwrap();
        t.write(&Record::Done {
            at: now(),
            seconds: 2.0,
            error: None,
        })
        .unwrap();

        let saved = read(&t.path).unwrap();
        assert_eq!(saved.id, t.id);
        assert_eq!(saved.tasks, ["add a test"]);
        assert_eq!(saved.messages.len(), 2);
        assert_eq!(saved.context, 120);
        assert_eq!(saved.worker_usage.prompt_tokens, 100);
        assert_eq!(
            saved.exchanges(),
            [("add a test".to_string(), "done".to_string())]
        );

        // Appending after a reopen, and /clear, are honoured on the next read.
        let again = Transcript::open(&t.path).unwrap();
        again.write(&Record::Clear { at: now() }).unwrap();
        let saved = read(&t.path).unwrap();
        assert!(saved.messages.is_empty());
        assert_eq!(
            saved.tasks.len(),
            1,
            "the task list keeps the whole history"
        );
    }

    #[test]
    fn lists_newest_first_and_finds_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let a = Transcript::start(dir.path(), meta()).unwrap();
        let b = Transcript::start(dir.path(), meta()).unwrap();
        assert_ne!(
            a.id, b.id,
            "two sessions in one second still get distinct ids"
        );
        let all = list(dir.path()).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, b.id);
        assert_eq!(find(dir.path(), "last").unwrap().id, b.id);
        assert_eq!(find(dir.path(), &a.id).unwrap().id, a.id);
        assert!(find(dir.path(), "nope").is_err());
        assert!(list(&dir.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn slugs_are_flat_names() {
        assert_eq!(
            slug(Path::new("/Users/amie/Code/weft")),
            "-Users-amie-Code-weft"
        );
    }
}
