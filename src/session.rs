//! One session: the four model handles, the tools, the folder, and the
//! worker's conversation, shared between whatever front end is driving it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use crate::agent::tandem::{Mode, Tandem, TandemOutcome};
use crate::agent::worker::WorkerError;
use crate::agent::{Approver, Events, Model, prompts};
use crate::config::{Config, PlanWhen};
use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::lm::{Endpoint, Message};
use crate::tools::{Ctx, Registry};

pub struct Session {
    pub planner: ChatModel,
    pub reviewer: ChatModel,
    pub worker: RawModel,
    pub worker_thinking: RawModel,
    pub registry: Registry,
    pub ctx: Ctx,
    pub when: PlanWhen,
    pub review_rounds: u8,
    /// Tools approved for the whole session, carried from task to task.
    // Rust: a `Mutex` because the UI runs tasks on spawned tokio tasks that
    // share this struct through an `Arc`; the lock is only ever held briefly,
    // never across an `.await`.
    allowed: Mutex<HashSet<String>>,
    messages: Mutex<Vec<Message>>,
}

impl Session {
    pub fn new(config: &Config, root: PathBuf) -> Self {
        let endpoint = |model: &str| Endpoint {
            url: config.lm_studio.url.clone(),
            api_token: config.lm_studio.api_token.clone(),
            model: model.to_string(),
        };
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
            registry: Registry::builtin(),
            ctx: Ctx { root },
            when: config.planner.when,
            review_rounds: config.planner.review_rounds,
            allowed: Mutex::new(HashSet::new()),
            messages: Mutex::new(vec![Message::System(prompts::WORKER.into())]),
        }
    }

    pub fn planner_name(&self) -> &str {
        &self.planner.endpoint.model
    }

    pub fn worker_name(&self) -> &str {
        &self.worker.endpoint.model
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
        let before = messages.len();

        let worker: &dyn Model = if think {
            &self.worker_thinking
        } else {
            &self.worker
        };
        let mut tandem = Tandem {
            planner: &self.planner,
            reviewer: &self.reviewer,
            worker,
            worker_thinking: &self.worker_thinking,
            registry: &self.registry,
            ctx: &self.ctx,
            approver,
            events,
            cancel,
            allowed,
            when: self.when,
            review_rounds: self.review_rounds,
        };
        let result = tandem.run(task, mode, &mut messages).await;
        *self.allowed.lock().unwrap() = std::mem::take(&mut tandem.allowed);
        drop(tandem);

        if result.is_err() {
            messages.truncate(before);
        }
        *self.messages.lock().unwrap() = messages;
        result
    }

    /// Forget the conversation, keep the approvals.
    pub fn clear(&self) {
        self.messages.lock().unwrap().truncate(1);
    }

    /// How many messages the worker's conversation holds, system prompt aside.
    pub fn depth(&self) -> usize {
        self.messages.lock().unwrap().len().saturating_sub(1)
    }
}
