//! One session: the four model handles, the tools, the folder, and the
//! worker's conversation, shared between whatever front end is driving it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::compact::{self, Compacted};
use crate::agent::delegate::Delegate;
use crate::agent::tandem::{Mode, Tandem, TandemOutcome, snapshot};
use crate::agent::worker::WorkerError;
use crate::agent::{AgentEvent, Approver, Events, Model, Role, prompts};
use crate::checkpoint::Checkpoint;
use crate::config::{Config, Confirm, PlanWhen};
use crate::hooks::{self, Hooks, On};
use crate::lm::Message;
use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::mcp;
use crate::models::Models;
use crate::tools::memory;
use crate::tools::{Ctx, Registry, Rules};
use crate::transcript::{self, Record, Saved, Transcript};

pub struct Session {
    pub planner: ChatModel,
    pub reviewer: ChatModel,
    pub worker: RawModel,
    pub worker_thinking: RawModel,
    /// The worker's tools: built-in, scout, MCP.
    pub registry: Registry,
    /// The planner's: read-only built-ins and the scout.
    pub planner_registry: Registry,
    /// Which model plays which role; also builds the tools that need one.
    pub models: Models,
    /// Compact once the worker's context reaches this; 0 never.
    pub compact_at: u32,
    /// The config's hooks; the registry runs the edit and shell ones, the session the task ones.
    hooks: Hooks,
    rules: Rules,
    /// The worker's context window, in tokens.
    pub window: u32,
    pub ctx: Ctx,
    pub when: PlanWhen,
    pub review_rounds: u8,
    pub confirm: Confirm,
    /// The MCP servers that were configured and how each came up.
    pub mcp: Vec<mcp::Status>,
    /// Things worth a line on the screen at the start: a model not loaded.
    pub warnings: Vec<String>,
    /// The servers themselves, kept so their processes live as long as the session.
    servers: Vec<mcp::Server>,
    /// Tools approved for the whole session. The running task's workers and
    /// helpers share this very set through the tool context, so an `a` on a
    /// prompt, a helper's `a`, and `/accept` typed mid-task all land in one
    /// place and take effect at once.
    // Rust: a `Mutex` because the UI runs tasks on spawned tokio tasks that
    // share this struct through an `Arc`; the lock is only ever held briefly,
    // never across an `.await`.
    allowed: Arc<Mutex<HashSet<String>>>,
    messages: Mutex<Vec<Message>>,
    /// The session file, once `start_transcript` has opened one.
    transcript: Mutex<Option<Arc<Transcript>>>,
    /// Whether this process created the file (rather than reopening one),
    /// and how many tasks it has run: an untouched new file is removed at the end.
    fresh_transcript: Mutex<bool>,
    tasks_run: AtomicUsize,
    /// A note for the worker above its next task, set by a resume or an undo.
    lead: Mutex<Option<String>>,
    /// One checkpoint per task that changed files, newest last, for `/undo`.
    checkpoints: Mutex<Vec<Arc<Checkpoint>>>,
}

impl Session {
    pub fn new(config: &Config, root: PathBuf) -> Self {
        let models = Models::from(config);
        // The extra tools are built afresh for each registry, so the planner's
        // and the worker's tool sets are independent.
        let mut registry = Registry::builtin()
            .with_rules(config.approve.clone())
            .with_hooks(config.hooks.clone());
        registry.extend(models.extra_tools(&root));
        let mut planner_registry = Registry::read_only();
        planner_registry.extend(models.extra_tools(&root));
        Self {
            planner: models.planner(),
            reviewer: models.reviewer(),
            worker: models.worker(false),
            worker_thinking: models.worker(true),
            registry,
            planner_registry,
            models,
            compact_at: config.compact.at,
            hooks: Hooks(config.hooks.clone()),
            rules: config.approve.clone(),
            window: config.compact.window,
            ctx: Ctx::new(root),
            when: config.planner.when,
            review_rounds: config.planner.review_rounds,
            confirm: config.planner.confirm,
            mcp: Vec::new(),
            warnings: Vec::new(),
            servers: Vec::new(),
            allowed: Arc::default(),
            messages: Mutex::new(vec![Message::System(prompts::WORKER.into())]),
            transcript: Mutex::new(None),
            fresh_transcript: Mutex::new(false),
            tasks_run: AtomicUsize::new(0),
            lead: Mutex::new(None),
            checkpoints: Mutex::new(Vec::new()),
        }
    }

    /// Where this task's kept copies go: beside the transcripts, per session and task.
    fn checkpoint_dir(&self) -> Option<std::path::PathBuf> {
        let home = std::env::home_dir()?;
        let session = self.transcript_id().unwrap_or_else(|| "session".into());
        let n = self.tasks_run.load(Ordering::Relaxed);
        Some(
            home.join(".local/share/weft/checkpoints")
                .join(transcript::slug(&self.ctx.root))
                .join(format!("{session}-{n}")),
        )
    }

    /// `/undo`: put back the files the last task changed. `None` when there is
    /// nothing to undo. The worker is told on its next task.
    pub fn undo(&self) -> anyhow::Result<Option<Vec<String>>> {
        let Some(cp) = self.checkpoints.lock().unwrap().pop() else {
            return Ok(None);
        };
        let files = cp.restore(&self.ctx.root)?;
        let mut lead = self.lead.lock().unwrap();
        let note = format!(
            "## Undone\nThe user put these files back as they were before your last task: {}. Your edits to them are gone; read a file again before editing it.\n\n",
            files.join(", ")
        );
        *lead = Some(match lead.take() {
            Some(earlier) => format!("{earlier}{note}"),
            None => note,
        });
        Ok(Some(files))
    }

    /// Pick up an earlier session: its conversation becomes the worker's, its
    /// file is appended to from here on, and the worker is told the project
    /// may have changed in the meantime.
    pub fn resume(&self, saved: &Saved) -> anyhow::Result<()> {
        let t = Transcript::open(&saved.path)?;
        t.write(&Record::Resumed {
            at: transcript::now(),
        })?;
        let mut messages = vec![Message::System(prompts::WORKER.into())];
        messages.extend(saved.messages.iter().cloned());
        *self.messages.lock().unwrap() = messages;
        *self.transcript.lock().unwrap() = Some(Arc::new(t));
        *self.fresh_transcript.lock().unwrap() = false;
        *self.lead.lock().unwrap() = Some(format!(
            "## Resumed\nThis conversation was saved {} and picked up again {}. The project may have changed since: read a file again before editing it, and do not trust earlier tool output to still hold.\n\n{}",
            day_of(&saved.last),
            day_of(&transcript::now()),
            snapshot(&self.ctx.root)
        ));
        Ok(())
    }

    /// Start writing this session down; returns its id.
    pub fn start_transcript(&self) -> anyhow::Result<String> {
        let dir = transcript::dir_for(&self.ctx.root)?;
        let t = Transcript::start(
            &dir,
            Record::Meta {
                id: String::new(),
                root: self.ctx.root.display().to_string(),
                started: transcript::now(),
                planner: self.planner_name().to_string(),
                worker: self.worker_name().to_string(),
                scout: self.models.scout.clone(),
            },
        )?;
        let id = t.id.clone();
        *self.transcript.lock().unwrap() = Some(Arc::new(t));
        *self.fresh_transcript.lock().unwrap() = true;
        Ok(id)
    }

    /// At the end: a session file this process created and never gave a task
    /// to is removed, so `/resume` lists only sessions that did something.
    pub fn finish(&self) {
        let fresh = *self.fresh_transcript.lock().unwrap();
        if fresh
            && self.tasks_run.load(Ordering::Relaxed) == 0
            && let Some(t) = self.transcript.lock().unwrap().take()
        {
            let _ = std::fs::remove_file(&t.path);
        }
        // Undo is for the session; the kept copies go with it.
        for cp in self.checkpoints.lock().unwrap().drain(..) {
            cp.discard();
        }
    }

    pub fn transcript_id(&self) -> Option<String> {
        self.transcript
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.id.clone())
    }

    /// One line to the transcript. A write that fails once the file is open
    /// (a full disk) loses that line and nothing else; the session goes on.
    fn record(&self, record: Record) {
        if let Some(t) = &*self.transcript.lock().unwrap() {
            let _ = t.write(&record);
        }
    }

    /// Offer the tools of the servers that came up to the worker, and remember
    /// how every server fared for `/mcp`.
    pub fn attach_mcp(&mut self, connected: Vec<(mcp::Status, Option<mcp::Server>)>) {
        // The helpers' tool set: everything the worker has except `delegate`.
        let mut helpers = Registry::builtin()
            .with_rules(self.rules.clone())
            .with_hooks(self.hooks.0.clone());
        helpers.extend(self.models.extra_tools(&self.ctx.root));
        for (status, server) in connected {
            self.mcp.push(status);
            if let Some(server) = server {
                self.registry.extend(server.tools());
                helpers.extend(server.tools());
                self.servers.push(server);
            }
        }
        self.registry.extend(vec![Box::new(Delegate::new(
            Box::new(self.models.worker(false)),
            Arc::new(helpers),
        ))]);
    }

    /// Where `remember` writes, for `/memory`.
    pub fn memory_file(&self) -> Option<std::path::PathBuf> {
        memory::file_for(&self.ctx.root)
    }

    pub fn planner_name(&self) -> &str {
        &self.planner.endpoint.model
    }

    pub fn worker_name(&self) -> &str {
        &self.worker.endpoint.model
    }

    pub fn scout_name(&self) -> &str {
        &self.models.scout
    }

    /// Run one task through the tandem. The conversation is taken out for the
    /// duration and put back after; a failed or stopped task leaves it as it was.
    pub async fn run_task(
        &self,
        task: &str,
        mode: Mode,
        think: bool,
        approver: Arc<dyn Approver>,
        events: Events,
        cancel: CancellationToken,
    ) -> Result<TandemOutcome, WorkerError> {
        let mut messages = std::mem::take(&mut *self.messages.lock().unwrap());
        let allowed = self.allowed.lock().unwrap().clone();
        let lead = self.lead.lock().unwrap().take();
        let before = messages.len();
        let began = Instant::now();
        self.tasks_run.fetch_add(1, Ordering::Relaxed);
        self.record(Record::Task {
            at: transcript::now(),
            text: task.to_string(),
        });

        let worker: &dyn Model = if think {
            &self.worker_thinking
        } else {
            &self.worker
        };
        // The events pass through here on their way to the screen, so the
        // transcript sees each one too (the streamed text aside: the whole
        // answer lands in the conversation anyway).
        let (inner, mut inner_rx) = mpsc::channel::<AgentEvent>(256);
        let transcript = self.transcript.lock().unwrap().clone();
        // The worker's last prompt size, read off its turns as they end.
        let context = Arc::new(AtomicU32::new(0));
        let seen = context.clone();
        let forward = tokio::spawn(async move {
            while let Some(evt) = inner_rx.recv().await {
                if let AgentEvent::TurnEnd {
                    role: Role::Worker,
                    usage,
                    ..
                } = &evt
                {
                    seen.store(
                        usage.prompt_tokens + usage.completion_tokens,
                        Ordering::Relaxed,
                    );
                }
                if let Some(t) = &transcript
                    && !matches!(evt, AgentEvent::Text { .. } | AgentEvent::Reasoning { .. })
                {
                    let _ = t.write(&Record::Event {
                        at: transcript::now(),
                        event: evt.clone(),
                    });
                }
                if events.send(evt).await.is_err() {
                    break;
                }
            }
        });
        // The tools learn this task's stop signal, event stream and checkpoint.
        let checkpoint = self.checkpoint_dir().map(|d| Arc::new(Checkpoint::new(d)));
        if let Some(cp) = &checkpoint {
            cp.note_git_before(&self.ctx.root);
        }
        let shared = self.allowed.clone();
        let ctx = self
            .ctx
            .for_task(Some(inner.clone()), cancel.clone())
            .with_checkpoint(checkpoint.clone())
            .with_approver(approver.clone())
            .with_allowed(shared.clone());
        let mut tandem = Tandem {
            planner: &self.planner,
            reviewer: &self.reviewer,
            worker,
            worker_thinking: &self.worker_thinking,
            registry: &self.registry,
            planner_registry: &self.planner_registry,
            ctx: &ctx,
            approver: approver.as_ref(),
            events: inner.clone(),
            cancel: cancel.clone(),
            allowed,
            when: self.when,
            review_rounds: self.review_rounds,
            confirm: self.confirm,
            lead: lead.clone().unwrap_or_default(),
            window: self.window,
        };
        let result = tandem.run(task, mode, &mut messages).await;
        // What the task's own worker approved joins the shared set.
        shared
            .lock()
            .unwrap()
            .extend(std::mem::take(&mut tandem.allowed));
        drop(tandem);

        match &result {
            Ok(_) => {
                for m in &messages[before..] {
                    self.record(Record::Message { message: m.clone() });
                }
            }
            Err(_) => {
                // The note goes with the next task instead.
                messages.truncate(before);
                *self.lead.lock().unwrap() = lead;
            }
        }

        // A conversation near the window is folded now, between tasks, so the
        // next one starts with room. A failure here is left for next time.
        let used = context.load(Ordering::Relaxed);
        if result.is_ok() && self.compact_at > 0 && used >= self.compact_at {
            let _ = inner
                .send(AgentEvent::Phase {
                    name: "compacting".into(),
                })
                .await;
            if let Ok(done) = self
                .fold(&ctx, inner.clone(), cancel.clone(), &mut messages, used)
                .await
            {
                let _ = inner.send(done.event()).await;
            }
        }
        // after_task hooks: a check of your own once the files are in place.
        if let Ok(out) = &result
            && self.hooks.has(On::AfterTask)
        {
            let touched = out.touched.join(",");
            for o in self
                .hooks
                .run(On::AfterTask, &self.ctx.root, &[("WEFT_TOUCHED", &touched)])
                .await
            {
                let _ = inner
                    .send(AgentEvent::Hook {
                        ok: o.ok,
                        text: hooks::describe(std::slice::from_ref(&o)),
                    })
                    .await;
            }
        }
        drop(ctx);
        drop(inner);
        let _ = forward.await;
        if let Some(cp) = checkpoint {
            cp.note_git_after(&self.ctx.root);
            if cp.is_empty() {
                cp.discard();
            } else {
                self.checkpoints.lock().unwrap().push(cp);
            }
        }
        self.record(Record::Done {
            at: transcript::now(),
            seconds: began.elapsed().as_secs_f32(),
            error: result.as_ref().err().map(|e| e.to_string()),
        });
        *self.messages.lock().unwrap() = messages;
        result
    }

    /// `/compact`: fold the earlier tasks into a note now. `before` is the
    /// worker's context as the screen knows it, for the report.
    pub async fn compact(
        &self,
        events: Events,
        cancel: CancellationToken,
        before: u32,
    ) -> Result<Compacted, WorkerError> {
        let mut messages = std::mem::take(&mut *self.messages.lock().unwrap());
        let ctx = self.ctx.for_task(Some(events.clone()), cancel.clone());
        let result = self.fold(&ctx, events, cancel, &mut messages, before).await;
        *self.messages.lock().unwrap() = messages;
        result
    }

    async fn fold(
        &self,
        ctx: &Ctx,
        events: Events,
        cancel: CancellationToken,
        messages: &mut Vec<Message>,
        before: u32,
    ) -> Result<Compacted, WorkerError> {
        let done = compact::compact(
            self.models.scout().as_ref(),
            ctx,
            events,
            cancel,
            messages,
            before,
        )
        .await?;
        self.record(Record::Replace {
            at: transcript::now(),
            note: format!(
                "compacted: {} tasks folded into a note at {} tokens",
                done.tasks, done.before
            ),
            messages: messages[1..].to_vec(),
        });
        Ok(done)
    }

    /// Let these tools run without asking for the rest of the session, or ask again.
    pub fn allow(&self, tools: &[&str], yes: bool) {
        let mut allowed = self.allowed.lock().unwrap();
        for t in tools {
            if yes {
                allowed.insert((*t).to_string());
            } else {
                allowed.remove(*t);
            }
        }
    }

    /// Forget the conversation, keep the approvals.
    pub fn clear(&self) {
        self.messages.lock().unwrap().truncate(1);
        self.record(Record::Clear {
            at: transcript::now(),
        });
    }

    /// The messages the worker would see next, for the screen to replay.
    pub fn messages(&self) -> Vec<Message> {
        self.messages.lock().unwrap().clone()
    }

    /// How many messages the worker's conversation holds, system prompt aside.
    pub fn depth(&self) -> usize {
        self.messages.lock().unwrap().len().saturating_sub(1)
    }
}

/// `2026-10-25T14:19:01-07:00` → `on 2026-10-25 at 14:19`.
fn day_of(at: &str) -> String {
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(time)) => format!("on {day} at {time}"),
        _ => "earlier".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
[lm_studio]
[planner]
model = "p"
[worker]
model = "w"
"#,
        )
        .unwrap()
    }

    #[test]
    fn resume_restores_the_conversation_and_warns_the_worker_once() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(&config(), dir.path().to_path_buf());
        let sessions = dir.path().join("sessions");
        let t = Transcript::start(
            &sessions,
            Record::Meta {
                id: String::new(),
                root: dir.path().display().to_string(),
                started: transcript::now(),
                planner: "p".into(),
                worker: "w".into(),
                scout: "w".into(),
            },
        )
        .unwrap();
        t.write(&Record::Message {
            message: Message::User("sort the tiles".into()),
        })
        .unwrap();
        t.write(&Record::Message {
            message: Message::Assistant {
                content: "Sorted.".into(),
                reasoning: None,
                tool_calls: vec![],
            },
        })
        .unwrap();
        let saved = transcript::read(&t.path).unwrap();

        assert_eq!(session.depth(), 0);
        session.resume(&saved).unwrap();
        assert_eq!(session.depth(), 2);
        assert!(matches!(&session.messages()[0], Message::System(_)));
        assert_eq!(session.transcript_id().as_deref(), Some(saved.id.as_str()));
        let lead = session.lead.lock().unwrap().clone().unwrap();
        assert!(
            lead.starts_with("## Resumed\n") && lead.contains("## Project\n"),
            "{lead}"
        );
        // The file got the marker, so a later read knows it was reopened.
        let again = transcript::read(&t.path).unwrap();
        assert_eq!(again.messages.len(), 2);
        assert_eq!(
            day_of("2026-10-25T14:19:01-07:00"),
            "on 2026-10-25 at 14:19"
        );
    }

    #[test]
    fn allow_reaches_the_set_a_running_task_reads() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(&config(), dir.path().to_path_buf());
        // The context a task gets shares the session's set, so a later
        // `/accept` is visible through it without any handing back.
        let ctx = session
            .ctx
            .for_task(None, Default::default())
            .with_allowed(session.allowed.clone());
        assert!(!ctx.allowed.lock().unwrap().contains("edit_file"));
        session.allow(&["edit_file", "write_file"], true);
        assert!(ctx.allowed.lock().unwrap().contains("edit_file"));
        session.allow(&["edit_file"], false);
        assert!(!ctx.allowed.lock().unwrap().contains("edit_file"));
        assert!(ctx.allowed.lock().unwrap().contains("write_file"));
    }
}
