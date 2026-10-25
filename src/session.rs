//! One session: the four model handles, the tools, the folder, and the
//! worker's conversation, shared between whatever front end is driving it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use tokio_util::sync::CancellationToken;

use crate::agent::scout::Scout;
use crate::agent::tandem::{Mode, Tandem, TandemOutcome};
use crate::agent::worker::WorkerError;
use crate::agent::{Approver, Events, Model, prompts};
use crate::config::{Config, PlanWhen};
use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::lm::{Endpoint, Message};
use crate::mcp;
use crate::tools::{Ctx, Registry};

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
        // three minutes on one question. Another model goes on the chat route.
        let scout_model = config
            .scout
            .model
            .clone()
            .unwrap_or_else(|| config.worker.model.clone());
        // One scout each, so the planner's and the worker's tool sets are independent.
        let scout = || {
            let model: Box<dyn Model> = if scout_model == config.worker.model {
                Box::new(RawModel::new(endpoint(&scout_model), false))
            } else {
                Box::new(ChatModel::new(
                    endpoint(&scout_model),
                    Some(config.scout.effort),
                ))
            };
            Box::new(Scout::new(model)) as Box<dyn crate::tools::Tool>
        };
        let mut registry = Registry::builtin();
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
            scout_model,
            ctx: Ctx::new(root),
            when: config.planner.when,
            review_rounds: config.planner.review_rounds,
            mcp: Vec::new(),
            servers: Vec::new(),
            allowed: Mutex::new(HashSet::new()),
            messages: Mutex::new(vec![Message::System(prompts::WORKER.into())]),
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
        let before = messages.len();

        let worker: &dyn Model = if think {
            &self.worker_thinking
        } else {
            &self.worker
        };
        // The tools learn this task's stop signal and event stream.
        let ctx = self.ctx.for_task(Some(events.clone()), cancel.clone());
        let mut tandem = Tandem {
            planner: &self.planner,
            reviewer: &self.reviewer,
            worker,
            worker_thinking: &self.worker_thinking,
            registry: &self.registry,
            planner_registry: &self.planner_registry,
            ctx: &ctx,
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
