//! One session: the four model handles, the tools, the folder, and the
//! worker's conversation, shared between whatever front end is driving it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::compact::{self, Compacted};
use crate::agent::scout::Scout;
use crate::agent::tandem::{Mode, Tandem, TandemOutcome, snapshot};
use crate::agent::worker::WorkerError;
use crate::agent::{AgentEvent, Approver, Events, Model, Role, prompts};
use crate::config::{Config, PlanWhen};
use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::lm::{Endpoint, Message};
use crate::mcp;
use crate::tools::{Ctx, Registry};
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
    scout_model: String,
    /// The scout's model once more, for writing the compaction note.
    digest: Box<dyn Model>,
    /// Compact once the worker's context reaches this; 0 never.
    pub compact_at: u32,
    pub ctx: Ctx,
    pub when: PlanWhen,
    pub review_rounds: u8,
    /// The MCP servers that were configured and how each came up.
    pub mcp: Vec<mcp::Status>,
    /// The servers themselves, kept so their processes live as long as the session.
    servers: Vec<mcp::Server>,
    /// Tools approved for the whole session, carried from task to task.
    // Rust: a `Mutex` because the UI runs tasks on spawned tokio tasks that
    // share this struct through an `Arc`; the lock is only ever held briefly,
    // never across an `.await`.
    allowed: Mutex<HashSet<String>>,
    messages: Mutex<Vec<Message>>,
    /// The session file, once `start_transcript` has opened one.
    transcript: Mutex<Option<Arc<Transcript>>>,
    /// A note for the worker above its next task, set by a resume.
    lead: Mutex<Option<String>>,
}

impl Session {
    pub fn new(config: &Config, root: PathBuf) -> Self {
        let endpoint = |model: &str| Endpoint {
            url: config.lm_studio.url.clone(),
            api_token: config.lm_studio.api_token.clone(),
            model: model.to_string(),
        };
        // The scout runs on the worker's model by default: the fast one, on the
        // raw route with thinking off. A dense model at low effort took close to
        // three minutes on one question. Another model takes the raw route too
        // unless the config gives it an effort, which only the chat route carries.
        let scout_model = config
            .scout
            .model
            .clone()
            .unwrap_or_else(|| config.worker.model.clone());
        let helper = || -> Box<dyn Model> {
            match config.scout.effort {
                Some(effort) => Box::new(ChatModel::new(endpoint(&scout_model), Some(effort))),
                None => Box::new(RawModel::new(endpoint(&scout_model), false)),
            }
        };
        // One scout each, so the planner's and the worker's tool sets are independent.
        let scout = || Box::new(Scout::new(helper())) as Box<dyn crate::tools::Tool>;
        let mut registry = Registry::builtin().with_rules(config.approve.clone());
        registry.extend(vec![scout()]);
        let mut planner_registry = Registry::read_only();
        planner_registry.extend(vec![scout()]);
        Self {
            planner: ChatModel::new(
                endpoint(&config.planner.model),
                Some(config.planner.plan_effort),
            ),
            reviewer: ChatModel::new(
                endpoint(&config.planner.model),
                Some(config.planner.review_effort),
            ),
            worker: RawModel::new(endpoint(&config.worker.model), false),
            worker_thinking: RawModel::new(endpoint(&config.worker.model), true),
            registry,
            planner_registry,
            digest: helper(),
            compact_at: config.compact.at,
            scout_model,
            ctx: Ctx::new(root),
            when: config.planner.when,
            review_rounds: config.planner.review_rounds,
            mcp: Vec::new(),
            servers: Vec::new(),
            allowed: Mutex::new(HashSet::new()),
            messages: Mutex::new(vec![Message::System(prompts::WORKER.into())]),
            transcript: Mutex::new(None),
            lead: Mutex::new(None),
        }
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
                scout: self.scout_model.clone(),
            },
        )?;
        let id = t.id.clone();
        *self.transcript.lock().unwrap() = Some(Arc::new(t));
        Ok(id)
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
        for (status, server) in connected {
            self.mcp.push(status);
            if let Some(server) = server {
                self.registry.extend(server.tools());
                self.servers.push(server);
            }
        }
    }

    pub fn planner_name(&self) -> &str {
        &self.planner.endpoint.model
    }

    pub fn worker_name(&self) -> &str {
        &self.worker.endpoint.model
    }

    pub fn scout_name(&self) -> &str {
        &self.scout_model
    }

    /// Run one task through the tandem. The conversation is taken out for the
    /// duration and put back after; a failed or stopped task leaves it as it was.
    pub async fn run_task(
        &self,
        task: &str,
        mode: Mode,
        think: bool,
        approver: &dyn Approver,
        events: Events,
        cancel: CancellationToken,
    ) -> Result<TandemOutcome, WorkerError> {
        let mut messages = std::mem::take(&mut *self.messages.lock().unwrap());
        let allowed = std::mem::take(&mut *self.allowed.lock().unwrap());
        let lead = self.lead.lock().unwrap().take();
        let before = messages.len();
        let began = Instant::now();
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
        // The tools learn this task's stop signal and event stream.
        let ctx = self.ctx.for_task(Some(inner.clone()), cancel.clone());
        let mut tandem = Tandem {
            planner: &self.planner,
            reviewer: &self.reviewer,
            worker,
            worker_thinking: &self.worker_thinking,
            registry: &self.registry,
            planner_registry: &self.planner_registry,
            ctx: &ctx,
            approver,
            events: inner.clone(),
            cancel: cancel.clone(),
            allowed,
            when: self.when,
            review_rounds: self.review_rounds,
            lead: lead.clone().unwrap_or_default(),
        };
        let result = tandem.run(task, mode, &mut messages).await;
        *self.allowed.lock().unwrap() = std::mem::take(&mut tandem.allowed);
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
        drop(ctx);
        drop(inner);
        let _ = forward.await;
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
        let done =
            compact::compact(self.digest.as_ref(), ctx, events, cancel, messages, before).await?;
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
}
