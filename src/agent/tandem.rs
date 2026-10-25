//! The two models in tandem: the planner looks and writes a plan, the worker
//! carries it out, the reviewer reads the diff and may send the worker back.

use std::collections::HashSet;
use std::path::Path;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::worker::{MAX_ROUNDS, Outcome, Worker, WorkerError};
use super::{AgentEvent, Approver, Events, Model, Role, Yes, prompts};
use crate::config::PlanWhen;
use crate::lm::{Message, Usage};
use crate::tools::{Ctx, Registry};

/// Tool rounds the planner gets to look around before it must write the plan.
const PLANNER_ROUNDS: usize = 8;
/// The diff shown to the reviewer is cut here, in characters.
const DIFF_LIMIT: usize = 30_000;

/// Whether to plan this task: the caller's say, or triage decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Plan,
    Go,
}

/// What a whole task amounted to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TandemOutcome {
    pub plan: Option<String>,
    pub answer: String,
    /// Each review: approved or not, and what it said.
    pub reviews: Vec<(bool, String)>,
    /// The reviewer asked for fixes after the last allowed round, so they were not made.
    pub fixes_pending: bool,
    pub touched: Vec<String>,
    pub rounds: usize,
    pub planner_usage: Usage,
    pub worker_usage: Usage,
}

pub struct Tandem<'a> {
    /// The planner at plan effort.
    pub planner: &'a dyn Model,
    /// The same model at review effort.
    pub reviewer: &'a dyn Model,
    /// The worker, thinking off.
    pub worker: &'a dyn Model,
    /// The worker, thinking on: used for fix rounds.
    pub worker_thinking: &'a dyn Model,
    pub registry: &'a Registry,
    pub ctx: &'a Ctx,
    pub approver: &'a dyn Approver,
    pub events: Events,
    pub cancel: CancellationToken,
    pub allowed: HashSet<String>,
    pub when: PlanWhen,
    pub review_rounds: u8,
}

impl Tandem<'_> {
    /// Run one task against the worker's conversation (`messages`, extended in place).
    pub async fn run(
        &mut self,
        task: &str,
        mode: Mode,
        messages: &mut Vec<Message>,
    ) -> Result<TandemOutcome, WorkerError> {
        let mut out = TandemOutcome::default();

        // 1. Plan, if this task warrants one.
        let plan = match self.should_plan(task, mode).await? {
            false => None,
            true => {
                self.phase("planning").await;
                let read_only = Registry::read_only();
                let mut planner =
                    self.looker(self.planner, &read_only, Role::Planner, PLANNER_ROUNDS);
                let mut convo = vec![
                    Message::System(prompts::PLANNER.into()),
                    Message::User(task.to_string()),
                ];
                let plan = planner.run(&mut convo).await?;
                out.planner_usage = add(out.planner_usage, plan.usage);
                self.emit(AgentEvent::Plan {
                    text: plan.answer.clone(),
                })
                .await;
                Some(plan.answer)
            }
        };

        out.plan = plan.clone();

        // 2. Work.
        self.phase("working").await;
        let ask = match &plan {
            Some(p) => format!("{task}\n\n{}\n\n{p}", prompts::WORKER_WITH_PLAN),
            None => task.to_string(),
        };
        messages.push(Message::User(ask));
        let done = self.work(self.worker, messages).await?;
        out.take(&done);

        // 3. Review what was changed, and send the worker back if asked.
        let mut round = 0;
        while !out.touched.is_empty() && round < self.review_rounds {
            round += 1;
            self.phase("reviewing").await;
            let diff = diff_of(&self.ctx.root, &out.touched).await;
            let brief = review_brief(task, plan.as_deref(), &out.answer, &diff);
            let empty = Registry::empty();
            let mut reviewer = self.looker(self.reviewer, &empty, Role::Planner, 1);
            let mut convo = vec![
                Message::System(prompts::REVIEWER.into()),
                Message::User(brief),
            ];
            let review = reviewer.run(&mut convo).await?;
            out.planner_usage = add(out.planner_usage, review.usage);
            let (approved, text) = verdict(&review.answer);
            self.emit(AgentEvent::Review {
                approved,
                text: text.clone(),
            })
            .await;
            out.reviews.push((approved, text.clone()));
            if approved {
                break;
            }
            if round == self.review_rounds {
                out.fixes_pending = true;
                break;
            }
            self.phase("fixing").await;
            messages.push(Message::User(format!("{}\n\n{text}", prompts::WORKER_FIX)));
            let fixed = self.work(self.worker_thinking, messages).await?;
            out.take(&fixed);
        }
        Ok(out)
    }

    async fn should_plan(&self, task: &str, mode: Mode) -> Result<bool, WorkerError> {
        Ok(match (mode, self.when) {
            (Mode::Plan, _) | (Mode::Auto, PlanWhen::Always) => true,
            (Mode::Go, _) | (Mode::Auto, PlanWhen::Never) => false,
            (Mode::Auto, PlanWhen::Auto) => {
                // A quick, silent word from the worker: its events go nowhere.
                let (quiet, _keep) = mpsc::channel(8);
                let empty = Registry::empty();
                let mut triage = Worker {
                    model: self.worker,
                    registry: &empty,
                    ctx: self.ctx,
                    approver: &Yes,
                    events: quiet,
                    cancel: self.cancel.clone(),
                    allowed: HashSet::new(),
                    role: Role::Worker,
                    max_rounds: 1,
                };
                let mut convo = vec![
                    Message::System(prompts::TRIAGE.into()),
                    Message::User(task.to_string()),
                ];
                let word = triage.run(&mut convo).await?.answer;
                word.trim_start().to_uppercase().starts_with("PLAN")
            }
        })
    }

    /// A worker over the real tools, carrying the session's approvals in and out.
    async fn work(
        &mut self,
        model: &dyn Model,
        messages: &mut Vec<Message>,
    ) -> Result<Outcome, WorkerError> {
        let mut worker = Worker {
            model,
            registry: self.registry,
            ctx: self.ctx,
            approver: self.approver,
            events: self.events.clone(),
            cancel: self.cancel.clone(),
            allowed: std::mem::take(&mut self.allowed),
            role: Role::Worker,
            max_rounds: MAX_ROUNDS,
        };
        let result = worker.run(messages).await;
        self.allowed = std::mem::take(&mut worker.allowed);
        result
    }

    /// A worker that only looks, for the planner and the reviewer.
    fn looker<'r>(
        &'r self,
        model: &'r dyn Model,
        registry: &'r Registry,
        role: Role,
        max_rounds: usize,
    ) -> Worker<'r> {
        Worker {
            model,
            registry,
            ctx: self.ctx,
            approver: &Yes,
            events: self.events.clone(),
            cancel: self.cancel.clone(),
            allowed: HashSet::new(),
            role,
            max_rounds,
        }
    }

    async fn phase(&self, name: &str) {
        self.emit(AgentEvent::Phase {
            name: name.to_string(),
        })
        .await;
    }

    async fn emit(&self, evt: AgentEvent) {
        let _ = self.events.send(evt).await;
    }
}

impl TandemOutcome {
    fn take(&mut self, done: &Outcome) {
        self.answer = done.answer.clone();
        self.rounds += done.rounds;
        self.worker_usage = add(self.worker_usage, done.usage);
        for path in &done.touched {
            if !self.touched.contains(path) {
                self.touched.push(path.clone());
            }
        }
    }
}

/// The reviewer's verdict and the rest of what it said. No verdict line reads
/// as approval: a rambling reviewer should not send the worker on a chase.
pub fn verdict(text: &str) -> (bool, String) {
    let mut approved = true;
    let mut rest = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t
            .strip_prefix("VERDICT:")
            .or_else(|| t.strip_prefix("**VERDICT:**"))
        {
            approved = v.to_uppercase().contains("APPROVE");
        } else {
            rest.push(line);
        }
    }
    let rest = rest.join("\n").trim().to_string();
    (approved, rest)
}

/// What the reviewer reads.
fn review_brief(task: &str, plan: Option<&str>, report: &str, diff: &str) -> String {
    let mut b = format!("## Task\n{task}\n\n");
    if let Some(p) = plan {
        b.push_str(&format!("## Plan\n{p}\n\n"));
    }
    b.push_str(&format!(
        "## Worker's report\n{report}\n\n## Diff\n```\n{diff}\n```"
    ));
    b
}

/// The changes to `paths` as a diff against git's HEAD, or the files whole
/// where git has nothing to compare (new files, no repository).
pub async fn diff_of(root: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let git = tokio::process::Command::new("git")
            .args(["diff", "HEAD", "--", path])
            .current_dir(root)
            .output()
            .await;
        let diff = match git {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => String::new(),
        };
        if !diff.trim().is_empty() {
            out.push_str(&diff);
        } else if let Ok(text) = std::fs::read_to_string(root.join(path)) {
            out.push_str(&format!("--- new or untracked file: {path}\n{text}\n"));
        }
        if out.len() > DIFF_LIMIT {
            out.truncate(DIFF_LIMIT);
            out.push_str("\n[... diff cut here]\n");
            break;
        }
    }
    if out.is_empty() {
        "(no changes found)".to_string()
    } else {
        out
    }
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
    use crate::lm::{LmError, Reply, Request, Sender, StreamEvent, ToolCall};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    struct Scripted {
        name: &'static str,
        replies: Mutex<Vec<Reply>>,
        seen: Mutex<Vec<String>>, // the last user message of each request
    }

    impl Scripted {
        fn new(name: &'static str, replies: Vec<Reply>) -> Self {
            Self {
                name,
                replies: Mutex::new(replies),
                seen: Mutex::new(vec![]),
            }
        }
        fn text(s: &str) -> Reply {
            Reply {
                content: s.into(),
                ..Default::default()
            }
        }
        fn calls(calls: Vec<ToolCall>) -> Reply {
            Reply {
                tool_calls: calls,
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl Model for Scripted {
        async fn stream(&self, req: &Request, tx: &Sender) -> Result<Reply, LmError> {
            let last = req.messages.iter().rev().find_map(|m| match m {
                Message::User(s) => Some(s.clone()),
                _ => None,
            });
            self.seen.lock().unwrap().push(last.unwrap_or_default());
            // The guard must be gone before the await below, or the future is not Send.
            let reply = {
                let mut replies = self.replies.lock().unwrap();
                assert!(
                    !replies.is_empty(),
                    "{} was asked more than it was scripted for",
                    self.name
                );
                replies.remove(0)
            };
            let _ = tx.send(StreamEvent::Text(reply.content.clone())).await;
            Ok(reply)
        }
        fn name(&self) -> &str {
            self.name
        }
    }

    fn edit(path: &str, old: &str, new: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "edit_file".into(),
            arguments: json!({"path": path, "old_string": old, "new_string": new}),
        }
    }

    #[tokio::test]
    async fn plans_works_reviews_and_fixes_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = Ctx {
            root: dir.path().to_path_buf(),
        };
        let registry = Registry::builtin();

        let planner = Scripted::new(
            "planner",
            vec![Scripted::text(
                "## Plan\n1. `a.txt`: say two\n\n## Verify\ncat a.txt",
            )],
        );
        let reviewer = Scripted::new(
            "reviewer",
            vec![
                Scripted::text("VERDICT: FIX\n1. `a.txt`: it should say three, not two"),
                Scripted::text("VERDICT: APPROVE\nNothing to add"),
            ],
        );
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::text("PLAN"),
                Scripted::calls(vec![edit("a.txt", "one", "two")]),
                Scripted::text("Changed to two."),
            ],
        );
        let worker_thinking = Scripted::new(
            "worker-thinking",
            vec![
                Scripted::calls(vec![edit("a.txt", "two", "three")]),
                Scripted::text("Changed to three."),
            ],
        );

        let (events, mut rx) = mpsc::channel(256);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker_thinking,
            registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Auto,
            review_rounds: 2,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("make it two", Mode::Auto, &mut messages)
            .await
            .unwrap();
        drop(tandem);

        assert!(out.plan.as_deref().unwrap().starts_with("## Plan"));
        assert_eq!(out.answer, "Changed to three.");
        assert_eq!(
            out.reviews,
            vec![
                (
                    false,
                    "1. `a.txt`: it should say three, not two".to_string()
                ),
                (true, "Nothing to add".to_string())
            ]
        );
        assert!(!out.fixes_pending);
        assert_eq!(out.touched, vec!["a.txt"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "three\n"
        );

        // The worker saw the task with the plan, then the reviewer's list.
        let seen = worker.seen.lock().unwrap().clone();
        assert_eq!(seen[0], "make it two", "triage saw the bare task");
        assert!(seen[1].starts_with("make it two\n\n") && seen[1].contains("## Plan"));
        let fix_seen = worker_thinking.seen.lock().unwrap().clone();
        assert!(
            fix_seen[0].starts_with(prompts::WORKER_FIX) && fix_seen[0].contains("three, not two")
        );
        // The reviewer saw the diff.
        let rev_seen = reviewer.seen.lock().unwrap().clone();
        assert!(
            rev_seen[0].contains("## Diff") && rev_seen[0].contains("two"),
            "{}",
            rev_seen[0]
        );

        let mut phases = Vec::new();
        while let Ok(evt) = rx.try_recv() {
            if let AgentEvent::Phase { name } = evt {
                phases.push(name);
            }
        }
        assert_eq!(
            phases,
            ["planning", "working", "reviewing", "fixing", "reviewing"]
        );
    }

    #[tokio::test]
    async fn go_skips_the_plan_and_an_answer_only_task_skips_review() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            root: dir.path().to_path_buf(),
        };
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![]);
        let reviewer = Scripted::new("reviewer", vec![]);
        let worker = Scripted::new("worker", vec![Scripted::text("It exports slugify.")]);
        let (events, _rx) = mpsc::channel(64);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Always,
            review_rounds: 1,
        };
        let mut messages = vec![];
        let out = tandem
            .run("what does it export?", Mode::Go, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.plan, None);
        assert_eq!(out.answer, "It exports slugify.");
        assert!(out.reviews.is_empty());
    }

    #[test]
    fn reads_verdicts() {
        assert_eq!(
            verdict("VERDICT: APPROVE\nNothing to add"),
            (true, "Nothing to add".into())
        );
        assert_eq!(
            verdict("  **VERDICT:** FIX\n1. a\n2. b"),
            (false, "1. a\n2. b".into())
        );
        assert_eq!(
            verdict("Looks fine to me."),
            (true, "Looks fine to me.".into())
        );
    }

    #[tokio::test]
    async fn diff_falls_back_to_whole_files_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.txt"), "new\n").unwrap();
        let d = diff_of(dir.path(), &["n.txt".into(), "missing.txt".into()]).await;
        assert_eq!(d, "--- new or untracked file: n.txt\nnew\n\n");
    }
}
