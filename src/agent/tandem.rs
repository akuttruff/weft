//! The two models in tandem: the planner looks and writes a plan, the worker
//! carries it out, the reviewer reads the diff and may send the worker back.

use std::collections::HashSet;
use std::path::Path;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::convo::{exchanges, task_message};
use super::proof::{self, Proof, Without, is_test_file};
use super::report::{self, Report, coverage};
use super::worker::{Outcome, Worker, WorkerError};
use super::{AgentEvent, Approver, Events, Model, PlanDecision, Role, Yes, prompts};
use crate::config::{Confirm, PlanWhen};
use crate::lm::{Message, Usage};
use crate::tools::bash::run_shell;
use crate::tools::todo::{self, FIX, Status, TEST, Todo};
use crate::tools::{Ctx, Registry};

/// Tool rounds the planner gets to look around before it must write the plan.
const PLANNER_ROUNDS: usize = 8;
/// Times the worker is sent back to finish open todos before the task ends.
const CARRY_ON_ROUNDS: usize = 2;
/// Times a failing check goes back to the worker before the task moves on.
const CHECK_FIXES: usize = 3;
/// Lines of a check's output passed on: the end, where the failures and the
/// summary are.
const CHECK_LINES: usize = 80;
/// Tool rounds the reviewer gets to look past the diff.
const REVIEW_ROUNDS: usize = 6;
/// A checklist line is cut here, in characters.
const ITEM_CHARS: usize = 160;
/// The diff shown to the reviewer is cut here, in characters.
const DIFF_LIMIT: usize = 30_000;
/// How many times the user may send a plan back with notes.
const PLAN_REVISIONS: usize = 3;

/// Whether to plan this task: the caller's say, or triage decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Plan,
    Go,
}

/// What a whole task amounted to.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TandemOutcome {
    pub plan: Option<String>,
    pub answer: String,
    /// Each review: approved or not, and what it said.
    pub reviews: Vec<(bool, String)>,
    /// The reviewer asked for fixes after the last allowed round, so they were not made.
    pub fixes_pending: bool,
    /// The reviewer gave no verdict, asked twice: the change was not reviewed.
    pub review_inconclusive: bool,
    /// The project's checks, as they stood when the task ended: passed or
    /// not. `None` when the project has none or nothing was changed.
    pub checks: Option<bool>,
    pub touched: Vec<String>,
    pub rounds: usize,
    pub planner_usage: Usage,
    pub worker_usage: Usage,
    /// What the card shows when the task ends; `None` when it stopped at its plan.
    pub report: Option<Report>,
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
    /// The worker's tools.
    pub registry: &'a Registry,
    /// The planner's: looking only, plus the scout.
    pub planner_registry: &'a Registry,
    pub ctx: &'a Ctx,
    pub approver: &'a dyn Approver,
    pub events: Events,
    pub cancel: CancellationToken,
    pub allowed: HashSet<String>,
    pub when: PlanWhen,
    pub review_rounds: u8,
    /// When a plan waits for the user before the worker starts.
    pub confirm: Confirm,
    /// Text put above this task when the conversation is not fresh: the
    /// note a resumed session starts with. Empty most of the time.
    pub lead: String,
    /// The worker's context window, for the in-task guard.
    pub window: u32,
    /// The model that folds a long task's earlier rounds into a note.
    pub digest: Option<&'a dyn Model>,
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
        let began = std::time::Instant::now();
        // The planner, the triage and the reviewer do not see the worker's
        // conversation, so "now add a test for that" needs the earlier exchange.
        let earlier = earlier(messages);
        let project = snapshot(self.ctx);
        let git_before = git_changes(&self.ctx.root).await;
        // Steps an earlier task left open go to every model, so "continue"
        // picks them up rather than starting over.
        let todos_at_start = self.todos();
        let open_earlier: Vec<Todo> = todos_at_start
            .iter()
            .filter(|t| t.is_open())
            .cloned()
            .collect();
        let carried = if open_earlier.is_empty() {
            String::new()
        } else {
            prompts::carried_todos(&todo::checklist(&todos_at_start))
        };

        // 1. Plan, if this task warrants one.
        let context = format!("{project}{}{carried}", earlier.as_deref().unwrap_or(""));
        let plan = match self.should_plan(task, Some(&context), mode).await? {
            false => None,
            true => {
                self.phase("planning").await;
                let mut planner = self
                    .looker(
                        self.planner,
                        self.planner_registry,
                        Role::Planner,
                        PLANNER_ROUNDS,
                    )
                    .wrap_up_with(prompts::PLAN_NOW);
                let mut convo = vec![
                    Message::System(prompts::PLANNER.into()),
                    Message::User(with_earlier(task, Some(&context))),
                ];
                let mut plan = planner.run(&mut convo).await?;
                out.planner_usage = add(out.planner_usage, plan.usage);
                // The person decides from the plan's first part: one that
                // skipped it is asked for again, once.
                let missing = missing_sections(&plan.answer);
                if !missing.is_empty() {
                    self.emit(AgentEvent::Note {
                        text: format!(
                            "the plan left out {}; asking for it again",
                            missing.join(", ")
                        ),
                    })
                    .await;
                    convo.push(Message::User(prompts::plan_reshape(&missing)));
                    plan = planner.run(&mut convo).await?;
                    out.planner_usage = add(out.planner_usage, plan.usage);
                }
                self.emit(AgentEvent::Plan {
                    text: plan.answer.clone(),
                })
                .await;

                // Plan mode: the user reads the plan first and may send it
                // back with notes a few times, or stop here.
                let pause = match self.confirm {
                    Confirm::Always => true,
                    Confirm::Never => false,
                    // Every plan waits for a word unless the task said go
                    // (`/go`); all-auto answers it at once.
                    Confirm::Asked => mode != Mode::Go,
                };
                let mut revisions = 0;
                if pause {
                    loop {
                        match self.approver.review_plan(&plan.answer).await {
                            PlanDecision::Go => break,
                            PlanDecision::Stop => {
                                out.plan = Some(plan.answer);
                                out.answer = "Stopped at the plan.".into();
                                return Ok(out);
                            }
                            PlanDecision::Revise(notes) => {
                                revisions += 1;
                                self.phase("planning").await;
                                convo.push(Message::User(format!(
                                    "The user read the plan and says:\n{notes}\n\nRevise the plan to suit and write it out in full again, in the same shape."
                                )));
                                plan = planner.run(&mut convo).await?;
                                out.planner_usage = add(out.planner_usage, plan.usage);
                                self.emit(AgentEvent::Plan {
                                    text: plan.answer.clone(),
                                })
                                .await;
                                if revisions >= PLAN_REVISIONS {
                                    break;
                                }
                            }
                        }
                    }
                }
                Some(plan.answer)
            }
        };

        out.plan = plan.clone();

        // The plan's steps become the checklist, so the user sees them ticked
        // off even when the worker forgets to keep one.
        let steps = plan.as_deref().map(steps_of).unwrap_or_default();
        let seeded = match plan.as_deref().map(plan_items) {
            Some(items) if !items.is_empty() => {
                todo::set(self.ctx, items.into_iter().map(Todo::pending).collect()).await;
                true
            }
            _ => false,
        };

        // 2. Work.
        self.phase("working").await;
        // The worker sees the project note on its first task; after that the
        // conversation carries it.
        let fresh = !messages.iter().any(|m| matches!(m, Message::User(_)));
        let mut lead = if fresh {
            project.clone()
        } else {
            self.lead.clone()
        };
        if seeded {
            lead.push_str(prompts::SEEDED_TODOS);
        } else {
            lead.push_str(&carried);
        }
        if let Some(command) = &self.ctx.project.verify {
            lead.push_str(&prompts::checks_note(command));
        }
        messages.push(Message::User(task_message(&lead, task, plan.as_deref())));
        // A plan with a `## Tasks` section runs as rounds: the worker does
        // one, answers, and gets the next, so each starts with a fresh look
        // at the code and the window is trimmed between them.
        if steps.len() >= 2 {
            for (i, step) in steps.iter().enumerate() {
                self.mark(i, step, Status::InProgress).await;
                self.emit(AgentEvent::Step {
                    n: i + 1,
                    of: steps.len(),
                    text: step.clone(),
                })
                .await;
                if i > 0 {
                    messages.push(Message::User(prompts::step_message(
                        i + 1,
                        steps.len(),
                        step,
                    )));
                } else if let Some(Message::User(first)) = messages.last_mut() {
                    first.push_str("\n\n");
                    first.push_str(&prompts::step_message(1, steps.len(), step));
                }
                let step_from = messages.len();
                let done = self.work(self.worker, messages).await?;
                out.take(&done);
                self.mark(i, step, Status::Done).await;
                // A finished round's steps fold into a note before the next
                // begins, so the window carries a summary of each round, not
                // its every read and edit. The round's answer stays.
                if i + 1 < steps.len()
                    && let Some(digest) = self.digest
                {
                    let _ = super::compact::fold_task(
                        digest,
                        self.ctx,
                        self.events.clone(),
                        self.cancel.clone(),
                        messages,
                        step_from,
                        1,
                    )
                    .await;
                }
            }
            // The rounds are ticked off as they end; the tests the plan's
            // `Done when` asks for may still be open.
            self.carry_on(self.worker, messages, &todos_at_start, &mut out)
                .await?;
        } else {
            let done = self.work(self.worker, messages).await?;
            out.take(&done);
            self.carry_on(self.worker, messages, &todos_at_start, &mut out)
                .await?;
        }

        // 3. The project's checks, run by weft itself: a failure goes back to
        // the worker before any model reads the diff.
        self.collect_touched(&git_before, &mut out).await;
        let mut checks = self.check(messages, &git_before, &mut out).await?;

        // 4. Review what was changed, and send the worker back if asked.
        let mut round = 0;
        // The last review's `Done when:` block, and the evidence it read.
        let mut cover: Vec<String> = Vec::new();
        let mut proof: Option<Proof> = None;
        // The `Fix:` steps this task's reviews asked for.
        let mut asked: Vec<String> = Vec::new();
        while !out.touched.is_empty() && round < self.review_rounds {
            round += 1;
            // The new tests, run with the code as it was: each should fail.
            let found = self.prove(&out).await?;
            self.phase("reviewing").await;
            let diff = diff_of(&self.ctx.root, &out.touched).await;
            let brief = review_brief(
                &with_earlier(task, Some(&context)),
                plan.as_deref(),
                &out.answer,
                checks.as_deref(),
                &found.brief(),
                &diff,
            );
            proof = Some(found);
            // The reviewer may look past the diff: at the code around it, at
            // what the changed code's neighbours do, for names left behind.
            let mut reviewer = self
                .looker(
                    self.reviewer,
                    self.planner_registry,
                    Role::Planner,
                    REVIEW_ROUNDS,
                )
                .wrap_up_with(prompts::REVIEW_NOW);
            let mut convo = vec![
                Message::System(prompts::REVIEWER.into()),
                Message::User(brief),
            ];
            let review = reviewer.run(&mut convo).await?;
            out.planner_usage = add(out.planner_usage, review.usage);
            let (mut decided, mut text) = verdict(&review.answer);
            // No verdict (the reviewer ran out of looks mid-search, say):
            // asked once more, with no tools, for the verdict alone.
            if decided.is_none() {
                convo.push(Message::User(prompts::VERDICT_ONLY.into()));
                let empty = Registry::empty();
                let mut again = self.looker(self.reviewer, &empty, Role::Planner, 1);
                let second = again.run(&mut convo).await?;
                out.planner_usage = add(out.planner_usage, second.usage);
                (decided, text) = verdict(&second.answer);
            }
            // Which test covers each `Done when` line, for the result card.
            let (covered, rest) = coverage(&text);
            text = rest;
            if !covered.is_empty() {
                cover = covered;
            }
            let Some(approved) = decided else {
                // Not approval, and nothing to send the worker after.
                let said = if text.is_empty() {
                    "the reviewer gave no verdict, asked twice; the change was not reviewed"
                        .to_string()
                } else {
                    format!(
                        "the reviewer gave no verdict, asked twice; the change was not reviewed. It said:\n{text}"
                    )
                };
                self.emit(AgentEvent::Review {
                    approved: false,
                    text: said.clone(),
                    inconclusive: true,
                })
                .await;
                out.reviews.push((false, said));
                out.review_inconclusive = true;
                break;
            };
            self.emit(AgentEvent::Review {
                approved,
                text: text.clone(),
                inconclusive: false,
            })
            .await;
            out.reviews.push((approved, text.clone()));
            if approved {
                // An approved diff settles what the reviewer asked for before.
                let mut list = self.todos();
                let mut settled = false;
                for t in list
                    .iter_mut()
                    .filter(|t| t.is_open() && t.text.starts_with(FIX))
                {
                    t.status = Status::Done;
                    settled = true;
                }
                if settled {
                    todo::set(self.ctx, list).await;
                }
                break;
            }
            // The reviewer's asks join the checklist: ticked off in the fix
            // round, or left open for the next task when no round is left.
            // Asks from this task's earlier rounds that it did not make again,
            // having read the fixed diff, are settled.
            let new: Vec<String> = numbered(&text)
                .into_iter()
                .map(|fix| clip(&format!("{FIX}{fix}"), ITEM_CHARS))
                .collect();
            todo::set(self.ctx, settle(self.todos(), &asked, &new)).await;
            asked.extend(new);
            if round == self.review_rounds {
                out.fixes_pending = true;
                break;
            }
            self.phase("fixing").await;
            let before_fix = self.todos();
            messages.push(Message::User(format!("{}\n\n{text}", prompts::WORKER_FIX)));
            let fixed = self.work(self.worker_thinking, messages).await?;
            out.take(&fixed);
            self.carry_on(self.worker_thinking, messages, &before_fix, &mut out)
                .await?;
            self.collect_touched(&git_before, &mut out).await;
            checks = self.check(messages, &git_before, &mut out).await?;
        }

        // 5. The result, for the card the screen shows; a question that
        // changed nothing and had no plan gets none.
        if out.plan.is_none() && out.touched.is_empty() {
            return Ok(out);
        }
        let report = report::build(
            task,
            out.plan.as_deref(),
            &out,
            &self.todos(),
            &cover,
            proof.as_ref(),
            began.elapsed().as_secs(),
        );
        self.emit(AgentEvent::Finished {
            report: report.clone(),
        })
        .await;
        out.report = Some(report);
        Ok(out)
    }

    /// The tests this task added, run in a copy of the project with the
    /// code (not the tests) as it was before the task: a test that passes
    /// there does not test the change. Also the existing test lines changed.
    async fn prove(&mut self, out: &TandemOutcome) -> Result<Proof, WorkerError> {
        let root = self.ctx.root.clone();
        let mut new = Vec::new();
        let mut changed = Vec::new();
        let mut test_files = Vec::new();
        for f in out.touched.iter().filter(|f| is_test_file(f)) {
            let Some((diff, is_new)) = proof::file_diff(&root, f).await else {
                continue;
            };
            let found = proof::new_tests(f, &diff);
            if !found.is_empty() {
                test_files.push(f.clone());
            }
            new.extend(found);
            if !is_new {
                changed.extend(proof::changed_lines(f, &diff));
            }
        }
        if new.is_empty() {
            return Ok(Proof::found(new, changed, None));
        }
        let code: Vec<String> = out
            .touched
            .iter()
            .filter(|f| !is_test_file(f))
            .cloned()
            .collect();
        if code.is_empty() {
            let note = "only tests changed, so there is no code to put back";
            return Ok(Proof::found(new, changed, Some(note.into())));
        }
        let Some(command) = proof::test_command(&root, self.ctx.project.test.as_deref()) else {
            let note =
                "weft does not know how to run this project's tests; set `test` in its [[project]]";
            return Ok(Proof::found(new, changed, Some(note.into())));
        };
        let command = proof::with_files(&command, &test_files);
        self.phase("checking").await;
        let id = format!("without-{}", out.reviews.len());
        self.emit(AgentEvent::ToolStart {
            id: id.clone(),
            name: "checks".into(),
            summary: format!("Without the change {command}"),
            args: serde_json::json!({ "command": command }),
        })
        .await;
        let ran = tokio::select! {
            r = proof::run_without(
                &root,
                self.ctx.checkpoint.as_deref(),
                &code,
                &command,
                self.ctx.project.verify_timeout_s,
            ) => r,
            _ = self.cancel.cancelled() => return Err(WorkerError::Cancelled),
        };
        let found = match ran {
            Ok(text) => {
                let results = proof::read_results(&text, &new);
                let note = results.iter().all(|w| *w == Without::Unknown).then(|| {
                    "its output did not name the new tests; a verbose reporter lists each"
                        .to_string()
                });
                Proof {
                    tests: new.into_iter().zip(results).collect(),
                    changed,
                    note,
                }
            }
            Err(e) => Proof::found(new, changed, Some(format!("could not run them: {e}"))),
        };
        self.emit(AgentEvent::ToolDone {
            id,
            ok: found.note.is_none() && found.passing().is_empty(),
            output: without_summary(&found),
        })
        .await;
        Ok(found)
    }

    /// Every file the task changed: by the edit tools, by a helper (the
    /// checkpoint saw them), or through bash (an `rm`, a formatter): whatever
    /// git shows changed now that it did not before the task.
    async fn collect_touched(&self, git_before: &HashSet<String>, out: &mut TandemOutcome) {
        let mut found = self
            .ctx
            .checkpoint
            .as_ref()
            .map(|cp| cp.files())
            .unwrap_or_default();
        found.extend(changed_since(
            git_before,
            &git_changes(&self.ctx.root).await,
        ));
        for f in found {
            if !out.touched.contains(&f) {
                out.touched.push(f);
            }
        }
    }

    /// Run the project's checks (`verify` in its `[[project]]`) when the task
    /// changed something; on a failure, send the output back to the worker
    /// and try again, a few times. What the last run printed is returned for
    /// the reviewer: `Some` with the command's result, `None` with no checks.
    async fn check(
        &mut self,
        messages: &mut Vec<Message>,
        git_before: &HashSet<String>,
        out: &mut TandemOutcome,
    ) -> Result<Option<String>, WorkerError> {
        let Some(command) = self.ctx.project.verify.clone() else {
            return Ok(None);
        };
        if out.touched.is_empty() {
            return Ok(None);
        }
        let timeout = self.ctx.project.verify_timeout_s;
        for attempt in 0..=CHECK_FIXES {
            self.phase("checking").await;
            let id = format!("checks-{attempt}");
            self.emit(AgentEvent::ToolStart {
                id: id.clone(),
                name: "checks".into(),
                summary: format!("Checks {command}"),
                args: serde_json::json!({ "command": command }),
            })
            .await;
            let ran = tokio::select! {
                r = run_shell(&self.ctx.root, &command, timeout) => r,
                _ = self.cancel.cancelled() => return Err(WorkerError::Cancelled),
            };
            let (ok, text) = match ran {
                Ok(r) => (r.ok, r.text),
                Err(e) => (false, format!("error: {e}\n")),
            };
            self.emit(AgentEvent::ToolDone {
                id,
                ok,
                output: text.clone(),
            })
            .await;
            out.checks = Some(ok);
            let report = format!(
                "`{command}` {}\n{}",
                if ok { "passed" } else { "FAILED" },
                tail_lines(&text, CHECK_LINES)
            );
            if ok || attempt == CHECK_FIXES {
                return Ok(Some(report));
            }
            self.phase("fixing").await;
            let before_fix = self.todos();
            messages.push(Message::User(prompts::checks_failed(
                &command,
                &tail_lines(&text, CHECK_LINES),
            )));
            let fixed = self.work(self.worker_thinking, messages).await?;
            out.take(&fixed);
            self.carry_on(self.worker_thinking, messages, &before_fix, out)
                .await?;
            self.collect_touched(git_before, out).await;
        }
        Ok(None)
    }

    /// The session's checklist as it stands.
    fn todos(&self) -> Vec<Todo> {
        self.ctx.todos.lock().unwrap().clone()
    }

    /// Set the status of step `i`, `step`, when the list still has it there.
    async fn mark(&self, i: usize, step: &str, status: Status) {
        let mut list = self.todos();
        if let Some(t) = list.get_mut(i)
            && t.text == clip(step, ITEM_CHARS)
        {
            t.status = status;
            todo::set(self.ctx, list).await;
        }
    }

    /// The worker stopped with steps open on a list this task changed: send it
    /// back to finish them or say why it cannot, rather than end quietly
    /// halfway. A list it did not touch (a question about something else)
    /// is left alone, and a round that moves nothing on ends the asking.
    async fn carry_on(
        &mut self,
        model: &dyn Model,
        messages: &mut Vec<Message>,
        at_start: &[Todo],
        out: &mut TandemOutcome,
    ) -> Result<(), WorkerError> {
        for _ in 0..CARRY_ON_ROUNDS {
            let now = self.todos();
            let open = now.iter().filter(|t| t.is_open()).count();
            if open == 0 || now == at_start {
                return Ok(());
            }
            self.emit(AgentEvent::Note {
                text: format!(
                    "{open} todo{} still open; the worker carries on",
                    if open == 1 { "" } else { "s" }
                ),
            })
            .await;
            messages.push(Message::User(prompts::open_todos(&todo::checklist(&now))));
            let done = self.work(model, messages).await?;
            out.take(&done);
            if self.todos() == now {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn should_plan(
        &self,
        task: &str,
        earlier: Option<&str>,
        mode: Mode,
    ) -> Result<bool, WorkerError> {
        Ok(match (mode, self.when) {
            (Mode::Plan, _) | (Mode::Auto, PlanWhen::Always) => true,
            (Mode::Go, _) | (Mode::Auto, PlanWhen::Never) => false,
            (Mode::Auto, PlanWhen::Auto) => {
                // A quick, silent word: its events go nowhere. The small
                // model that folds notes (the scout's) answers when there is
                // one; one word does not need the worker's size, and a big
                // model can take minutes just to start.
                let (quiet, _keep) = mpsc::channel(8);
                let empty = Registry::empty();
                let model = self.digest.unwrap_or(self.worker);
                let mut triage = Worker::new(model, &empty, self.ctx, &Yes, quiet)
                    .rounds(1)
                    .window(self.window);
                triage.cancel = self.cancel.clone();
                let mut convo = vec![
                    Message::System(prompts::TRIAGE.into()),
                    Message::User(with_earlier(task, earlier)),
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
        let mut worker = Worker::new(
            model,
            self.registry,
            self.ctx,
            self.approver,
            self.events.clone(),
        )
        .allowed(std::mem::take(&mut self.allowed))
        .window(self.window)
        .digest(self.digest);
        worker.cancel = self.cancel.clone();
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
        let mut looker = Worker::new(model, registry, self.ctx, &Yes, self.events.clone())
            .role(role)
            .rounds(max_rounds)
            .window(self.window);
        looker.cancel = self.cancel.clone();
        looker
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

/// The numbered lines under a plan's `## Tasks` heading, if it has one.
pub fn steps_of(plan: &str) -> Vec<String> {
    numbered(section(plan, "## Tasks"))
}

/// The sections a plan must have: the person's part, and the worker's steps.
const PLAN_SECTIONS: [&str; 6] = [
    "## What you asked for",
    "## What will change for you",
    "## Choices to confirm",
    "## How it fits together",
    "## Done when",
    "## Steps",
];

/// The sections `plan` leaves out, by heading.
pub fn missing_sections(plan: &str) -> Vec<&'static str> {
    PLAN_SECTIONS
        .into_iter()
        .filter(|h| !plan.lines().any(|l| l.trim_end() == *h))
        .collect()
}

/// A plan in its two parts: the person's (what will change, the choices,
/// how it fits, when it's done) and the worker's, from `## Steps` on.
/// A plan without `## Steps` is all the person's.
pub fn split_plan(plan: &str) -> (&str, Option<&str>) {
    match plan.find("\n## Steps") {
        Some(at) => (plan[..at].trim_end(), Some(plan[at + 1..].trim())),
        None => (plan.trim_end(), None),
    }
}

/// The checklist a plan makes: its tasks when it has some, else its steps
/// (`## Plan` in plans written before the two-part shape), then a test for
/// each `## Done when` line, each cut to a line.
pub fn plan_items(plan: &str) -> Vec<String> {
    let tasks = steps_of(plan);
    let mut items = if tasks.len() >= 2 {
        tasks
    } else if plan.contains("## Steps") {
        numbered(section(plan, "## Steps"))
    } else {
        numbered(section(plan, "## Plan"))
    };
    items.extend(
        numbered(section(plan, "## Done when"))
            .into_iter()
            .map(|d| format!("{TEST}{d}")),
    );
    items.iter().map(|s| clip(s, ITEM_CHARS)).collect()
}

/// The text under `heading`, up to the next heading; empty without one.
pub(crate) fn section<'p>(plan: &'p str, heading: &str) -> &'p str {
    match plan.split_once(heading) {
        Some((_, after)) => after.split("\n## ").next().unwrap_or(""),
        None => "",
    }
}

/// `1. a` and `2) b` lines, without their numbers.
pub(crate) fn numbered(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|l| {
            let rest = l.trim_start_matches(|c: char| c.is_ascii_digit());
            (rest.len() < l.len() && (rest.starts_with('.') || rest.starts_with(')')))
                .then(|| rest[1..].trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// The reviewer's verdict and the rest of what it said: `Some(true)` to
/// approve, `Some(false)` to send back, `None` when no verdict line came.
/// Tool calls written out as text (a turn with no tools on offer) are not
/// part of what it said.
pub fn verdict(text: &str) -> (Option<bool>, String) {
    let mut decided = None;
    let mut rest = Vec::new();
    let text = without_calls(text);
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t
            .strip_prefix("VERDICT:")
            .or_else(|| t.strip_prefix("**VERDICT:**"))
        {
            decided = Some(v.to_uppercase().contains("APPROVE"));
        } else {
            rest.push(line);
        }
    }
    let rest = rest.join("\n").trim().to_string();
    (decided, rest)
}

/// `text` with any `<tool_call>…</tool_call>` blocks taken out, an unclosed
/// one to the end.
fn without_calls(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<tool_call>") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("</tool_call>") {
            Some(end) => &rest[start + end + "</tool_call>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The last few tasks and answers of this session, for the models that do not
/// share the worker's conversation. `None` on a fresh session.
pub fn earlier(messages: &[Message]) -> Option<String> {
    let pairs = exchanges(messages);
    if pairs.is_empty() {
        return None;
    }
    let keep = pairs.len().saturating_sub(3);
    let mut out = String::from("## Earlier in this session\n");
    for (t, a) in &pairs[keep..] {
        out.push_str(&format!("> {t}\n{}\n\n", clip(a, 1200)));
    }
    Some(out)
}

/// The task, with the project note and the earlier exchange above it.
fn with_earlier(task: &str, context: Option<&str>) -> String {
    match context {
        Some(c) if !c.is_empty() => format!("{c}## Task\n{task}"),
        _ => task.to_string(),
    }
}

/// A few lines on the folder weft is in: what every model is told first, so
/// an empty folder is a new project and not a mystery.
pub fn snapshot(ctx: &Ctx) -> String {
    let root = &ctx.root;
    let global = std::env::home_dir().map(|h| h.join(".config/weft/WEFT.md"));
    let kept = crate::tools::memory::file_for(root);
    let configured = ctx.project.notes.as_deref().unwrap_or("");
    format!(
        "{}{}",
        folder_note(root),
        notes(root, global.as_deref(), configured, kept.as_deref())
    )
}

/// How much of a notes file the models are shown, in characters.
const NOTES_CHARS: usize = 8_000;

/// The user's standing instructions: `~/.config/weft/WEFT.md` for every
/// project, `WEFT.md` in the folder and the config's `notes` for this one,
/// then what weft itself kept with `remember`, as a `## Project notes`
/// section. Empty when there are none.
pub fn notes(root: &Path, global: Option<&Path>, configured: &str, kept: Option<&Path>) -> String {
    let mut out = String::new();
    let mut add = |label: String, text: &str| {
        if !text.trim().is_empty() {
            out.push_str(&format!(
                "### Notes {label}\n{}\n\n",
                clip(text.trim(), NOTES_CHARS)
            ));
        }
    };
    let read = |p: Option<&Path>| {
        p.and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    let shown = |label: &str, p: Option<&Path>| match p {
        Some(p) => format!("{label} ({})", p.display()),
        None => label.to_string(),
    };
    let here = root.join("WEFT.md");
    add(shown("for every project", global), &read(global));
    add(shown("for this project", Some(&here)), &read(Some(&here)));
    add("for this project (weft's config)".into(), configured);
    add(shown("weft kept from earlier tasks", kept), &read(kept));
    if out.is_empty() {
        return out;
    }
    format!(
        "## Project notes\nThe user's standing instructions. Follow them; they outrank the usual defaults.\n\n{out}"
    )
}

fn folder_note(root: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    (name != ".git"
                        && name != "node_modules"
                        && name != "target"
                        && !name.starts_with(".DS"))
                    .then(|| if is_dir { format!("{name}/") } else { name })
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let git = root.join(".git").exists();
    let mut out = format!("## Project\nFolder: {}\n", root.display());
    if names.is_empty() {
        out.push_str(&format!(
            "The folder is empty{}: this is a new project. There is nothing to read; choose a sensible stack for the task, lay out the files, and say what you chose.\n\n",
            if git { " (a git repository)" } else { "" }
        ));
        return out;
    }
    let stack: Vec<&str> = [
        ("package.json", "Node"),
        ("Cargo.toml", "Rust"),
        ("pyproject.toml", "Python"),
        ("go.mod", "Go"),
        ("Gemfile", "Ruby"),
    ]
    .iter()
    .filter(|(f, _)| names.iter().any(|n| n == f))
    .map(|(_, s)| *s)
    .collect();
    if !stack.is_empty() {
        out.push_str(&format!("Stack: {}\n", stack.join(", ")));
    }
    out.push_str(if git { "Git: yes\n" } else { "Git: no\n" });
    let shown: Vec<&str> = names.iter().take(40).map(String::as_str).collect();
    out.push_str(&format!("Top level: {}", shown.join(" ")));
    if names.len() > 40 {
        out.push_str(&format!(" (+{} more)", names.len() - 40));
    }
    out.push('\n');
    out.push_str(&tree(root));
    out.push('\n');
    out
}

/// Files the tree lists before it is cut.
const TREE_FILES: usize = 150;
const TREE_PER_DIR: usize = 30;

/// The folders under the top level and what is in them, three levels deep,
/// one line each, so the models know the paths before they look. Hidden and
/// `.gitignore`d files are left out, as are `node_modules` and `target`.
fn tree(root: &Path) -> String {
    use std::collections::BTreeMap;
    let mut dirs: BTreeMap<std::path::PathBuf, Vec<String>> = BTreeMap::new();
    let walk = ignore::WalkBuilder::new(root)
        .max_depth(Some(3))
        .hidden(true)
        .require_git(false)
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some("node_modules" | "target" | ".git" | "dist" | "build")
            )
        })
        .build();
    let mut count = 0;
    let mut cut = false;
    for entry in walk.flatten() {
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) else {
            continue; // the top level is listed above
        };
        if count >= TREE_FILES {
            cut = true;
            break;
        }
        count += 1;
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        dirs.entry(parent.to_path_buf())
            .or_default()
            .push(if is_dir { format!("{name}/") } else { name });
    }
    let mut out = String::new();
    for (dir, mut names) in dirs {
        names.sort();
        let shown: Vec<&str> = names
            .iter()
            .take(TREE_PER_DIR)
            .map(String::as_str)
            .collect();
        out.push_str(&format!("{}/: {}", dir.display(), shown.join(" ")));
        if names.len() > TREE_PER_DIR {
            out.push_str(&format!(" (+{} more)", names.len() - TREE_PER_DIR));
        }
        out.push('\n');
    }
    if cut {
        out.push_str("(the tree is cut here; list_files shows the rest)\n");
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The run without the change, as the screen shows it: a line, then each test.
fn without_summary(p: &Proof) -> String {
    if let Some(note) = &p.note {
        return format!("not run: {note}");
    }
    let fails = p
        .tests
        .iter()
        .filter(|(_, w)| matches!(w, Without::Fails | Without::FileFails))
        .count();
    let mut s = format!(
        "{fails} of {} new test{} fail without the change",
        p.tests.len(),
        if p.tests.len() == 1 { "" } else { "s" }
    );
    for (t, w) in &p.tests {
        let mark = match w {
            Without::Fails | Without::FileFails => "✓",
            Without::Passes => "⚠",
            Without::Unknown => "?",
        };
        s.push_str(&format!("\n{mark} {}: {}", t.name, w.words()));
    }
    s
}

/// The checklist with the reviewer's new asks: `asked` (this task's earlier
/// asks) not among `new` are done; a new ask already listed is open again.
fn settle(mut list: Vec<Todo>, asked: &[String], new: &[String]) -> Vec<Todo> {
    for t in list
        .iter_mut()
        .filter(|t| t.is_open() && asked.contains(&t.text) && !new.contains(&t.text))
    {
        t.status = Status::Done;
    }
    for n in new {
        match list.iter_mut().find(|t| &t.text == n) {
            Some(t) => t.status = Status::Pending,
            None => list.push(Todo::pending(n.clone())),
        }
    }
    list
}

/// What the reviewer reads.
fn review_brief(
    task: &str,
    plan: Option<&str>,
    report: &str,
    checks: Option<&str>,
    proof: &str,
    diff: &str,
) -> String {
    let mut b = format!("## Task\n{task}\n\n");
    if let Some(p) = plan {
        b.push_str(&format!("## Plan\n{p}\n\n"));
    }
    b.push_str(&format!("## Worker's report\n{report}\n\n"));
    if let Some(c) = checks {
        b.push_str(&format!(
            "## Checks\nRun by weft just now, not by the worker:\n```\n{c}\n```\n\n"
        ));
    }
    b.push_str(proof);
    b.push_str(&format!("## Diff\n```\n{diff}\n```"));
    b
}

/// The last `n` lines of `text`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
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

/// `git status` entries (`XY path`), one per changed file. Empty outside git.
pub async fn git_changes(root: &Path) -> HashSet<String> {
    let git = tokio::process::Command::new("git")
        .args([
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ])
        .current_dir(root)
        .output()
        .await;
    match git {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split('\0')
            .filter(|e| e.len() > 3)
            .map(str::to_string)
            .collect(),
        _ => HashSet::new(),
    }
}

/// The paths whose `git status` entry is new since `before`, sorted.
fn changed_since(before: &HashSet<String>, after: &HashSet<String>) -> Vec<String> {
    let mut paths: Vec<String> = after
        .difference(before)
        .map(|e| e[3..].to_string())
        .collect();
    paths.sort();
    paths
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

    /// A plan in the two-part shape, with `steps` under `## Steps`.
    fn shaped(steps: &str) -> String {
        format!(
            "## What you asked for\nMake it so.\n\n## What will change for you\n- It is so.\n\n## Choices to confirm\nNone\n\n## How it fits together\n- One file.\n\n## Done when\nIt reads as asked.\n\n## Steps\n{steps}"
        )
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
        let ctx = Ctx::new(dir.path().to_path_buf());
        let mut registry = Registry::builtin();
        registry.extend(vec![Box::new(crate::tools::todo::TodoWrite)]);

        let planner = Scripted::new(
            "planner",
            vec![Scripted::text(&shaped(
                "1. `a.txt`: say two\n\n## Verify\ncat a.txt",
            ))],
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
                // Sent back: the plan's step is still open on the checklist.
                Scripted::calls(vec![ToolCall {
                    id: "t".into(),
                    name: "todo".into(),
                    arguments: json!({"todos": [{"text": "`a.txt`: say two", "status": "done"}]}),
                }]),
                Scripted::text("Ticked it off."),
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
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Auto,
            review_rounds: 2,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
            confirm: Confirm::Asked,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("make it two", Mode::Auto, &mut messages)
            .await
            .unwrap();
        drop(tandem);

        assert!(
            out.plan
                .as_deref()
                .unwrap()
                .starts_with("## What you asked for")
        );
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
        // The plan's step was ticked off by the worker, the reviewer's ask by
        // the approval that followed the fix.
        let list = ctx.todos.lock().unwrap().clone();
        assert_eq!(
            list.iter()
                .map(|t| (t.text.as_str(), t.status))
                .collect::<Vec<_>>(),
            [
                ("`a.txt`: say two", Status::Done),
                ("Fix: `a.txt`: it should say three, not two", Status::Done)
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "three\n"
        );

        // The worker saw the task with the plan, then the reviewer's list.
        let seen = worker.seen.lock().unwrap().clone();
        assert!(
            seen[0].starts_with("## Project\n") && seen[0].ends_with("## Task\nmake it two"),
            "triage saw the project note and the task: {}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("## Project\n")
                && seen[1].contains("make it two\n\n")
                && seen[1].contains("## Steps"),
            "{}",
            seen[1]
        );
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
        let seen = worker.seen.lock().unwrap().clone();
        assert!(
            seen[3].starts_with("## Open todos\nYou answered")
                && seen[3].contains("☐ `a.txt`: say two"),
            "{}",
            seen[3]
        );
    }

    #[tokio::test]
    async fn go_skips_the_plan_and_an_answer_only_task_skips_review() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
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
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Always,
            review_rounds: 1,
            confirm: Confirm::Asked,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
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

    #[tokio::test]
    async fn the_planner_hears_what_came_before() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![Scripted::text(&shaped("1. add a test"))]);
        let reviewer = Scripted::new("reviewer", vec![]);
        // The step is left open, so the worker is asked once more; a round
        // that changes nothing ends the asking.
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::text("Added the test."),
                Scripted::text("It is in."),
            ],
        );
        let (events, _rx) = mpsc::channel(64);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Always,
            review_rounds: 1,
            confirm: Confirm::Asked,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User(format!(
                "sort the tiles\n\n{}\n\n## Plan",
                prompts::WORKER_WITH_PLAN
            )),
            Message::Assistant {
                content: "Sorted them by status.".into(),
                reasoning: None,
                tool_calls: vec![],
            },
        ];
        tandem
            .run("now add a test for that", Mode::Plan, &mut messages)
            .await
            .unwrap();
        let seen = planner.seen.lock().unwrap().clone();
        assert!(
            seen[0].starts_with("## Project\n")
                && seen[0].contains(
                    "## Earlier in this session\n> sort the tiles\nSorted them by status."
                ),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].ends_with("## Task\nnow add a test for that"),
            "{}",
            seen[0]
        );
        assert_eq!(earlier(&[]), None);
    }

    #[test]
    fn exchanges_find_tasks_under_notes_and_skip_fix_requests() {
        let answer = |s: &str| Message::Assistant {
            content: s.into(),
            reasoning: None,
            tool_calls: vec![],
        };
        let messages = vec![
            Message::System("sys".into()),
            Message::User("## Project\nFolder: /p\n\n## Task\nsort the tiles\n\nplan…".into()),
            answer("Sorted."),
            Message::User(format!("{}\n\n- rename x", prompts::WORKER_FIX)),
            answer("Renamed."),
            Message::User("## Resumed\nThe project may have changed.".into()),
            answer("Noted."),
            Message::User("now add a test".into()),
            answer("Added one."),
        ];
        assert_eq!(
            exchanges(&messages),
            [
                ("sort the tiles".to_string(), "Sorted.".to_string()),
                ("now add a test".to_string(), "Added one.".to_string()),
            ]
        );
    }

    #[test]
    fn notes_come_from_the_folder_and_the_global_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(notes(dir.path(), None, "", None), "");
        std::fs::write(
            dir.path().join("WEFT.md"),
            "Run `cargo test` before answering.\n",
        )
        .unwrap();
        let global = dir.path().join("global.md");
        std::fs::write(&global, "One-line commits.").unwrap();
        let kept = dir.path().join("kept.md");
        std::fs::write(&kept, "- Tests take a minute.").unwrap();
        let text = notes(
            dir.path(),
            Some(&global),
            "Deploy only when asked.",
            Some(&kept),
        );
        assert!(
            text.contains("weft kept from earlier tasks") && text.contains("Tests take a minute.")
        );
        assert!(text.starts_with("## Project notes\n"), "{text}");
        assert!(
            text.contains("### Notes for this project (weft's config)\nDeploy only when asked."),
            "{text}"
        );
        let every = text.find("for every project").unwrap();
        let this = text.find("for this project").unwrap();
        assert!(every < this && text.contains("One-line commits.") && text.contains("cargo test"));
        let s = snapshot(&Ctx::new(dir.path().to_path_buf()));
        assert!(
            s.starts_with("## Project\n") && s.contains("## Project notes"),
            "{s}"
        );
    }

    /// A tandem over `ctx` with the project's checks set to `verify`, no plan.
    fn checked_ctx(dir: &Path, verify: &str) -> Ctx {
        let mut ctx = Ctx::new(dir.to_path_buf());
        ctx.project = std::sync::Arc::new(crate::config::Project {
            path: String::new(),
            verify: Some(verify.into()),
            verify_timeout_s: 30,
            ..Default::default()
        });
        ctx
    }

    #[tokio::test]
    async fn a_failing_check_goes_back_to_the_worker_and_the_reviewer_sees_it_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = checked_ctx(dir.path(), "test -f ok.txt");
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![]);
        let reviewer = Scripted::new(
            "reviewer",
            vec![Scripted::text("VERDICT: APPROVE\nNothing to add")],
        );
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::calls(vec![edit("a.txt", "one", "two")]),
                Scripted::text("Changed."),
            ],
        );
        let worker_thinking = Scripted::new(
            "worker-thinking",
            vec![
                Scripted::calls(vec![ToolCall {
                    id: "w".into(),
                    name: "write_file".into(),
                    arguments: json!({"path": "ok.txt", "content": "ok\n"}),
                }]),
                Scripted::text("Made the check pass."),
            ],
        );
        let (events, _rx) = mpsc::channel(256);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker_thinking,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Never,
            review_rounds: 1,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("make it two", Mode::Go, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.checks, Some(true));
        let seen = worker.seen.lock().unwrap().clone();
        assert!(
            seen[0].contains("## Checks\nWhen you are done, weft runs `test -f ok.txt`"),
            "{}",
            seen[0]
        );
        let fix = worker_thinking.seen.lock().unwrap().clone();
        assert!(
            fix[0].starts_with("## Checks failed\nweft ran `test -f ok.txt`")
                && fix[0].contains("[exit 1]"),
            "{}",
            fix[0]
        );
        let review = reviewer.seen.lock().unwrap().clone();
        assert!(
            review[0].contains("## Checks\nRun by weft")
                && review[0].contains("`test -f ok.txt` passed"),
            "{}",
            review[0]
        );
    }

    #[tokio::test]
    async fn the_new_tests_run_without_the_change_and_the_card_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let cp = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.txt"), "old\n").unwrap();
        let mut ctx = Ctx::new(dir.path().to_path_buf()).with_checkpoint(Some(
            std::sync::Arc::new(crate::checkpoint::Checkpoint::new(cp.path().join("cp"))),
        ));
        // A runner that marks each test, the first true only of the new code.
        ctx.project = std::sync::Arc::new(crate::config::Project {
            verify: Some("true".into()),
            test: Some(
                "grep -q new code.txt && echo '✓ says new' || echo '× says new'; echo '✓ keeps the rest'"
                    .into(),
            ),
            verify_timeout_s: 30,
            ..Default::default()
        });
        let mut registry = Registry::builtin();
        registry.extend(vec![Box::new(crate::tools::todo::TodoWrite)]);
        let plan = "## What you asked for\nSay new.\n\n## What will change for you\n- It says new.\n\n## Choices to confirm\nNone\n\n## How it fits together\n- One file.\n\n## Done when\n1. It says new.\n2. The rest stays.\n\n## Steps\n1. `code.txt`: say new";
        let planner = Scripted::new("planner", vec![Scripted::text(plan)]);
        let reviewer = Scripted::new(
            "reviewer",
            vec![Scripted::text(
                "VERDICT: APPROVE\nDone when:\n1. `check.test.js:1` \"says new\"\n2. `check.test.js:2` \"keeps the rest\"\n\nNothing to add",
            )],
        );
        let tests = "it('says new', () => {})\nit('keeps the rest', () => {})\n";
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::calls(vec![
                    edit("code.txt", "old", "new"),
                    ToolCall {
                        id: "w".into(),
                        name: "write_file".into(),
                        arguments: json!({"path": "check.test.js", "content": tests}),
                    },
                    ToolCall {
                        id: "t".into(),
                        name: "todo".into(),
                        arguments: json!({"todos": [
                            {"text": "`code.txt`: say new", "status": "done"},
                            {"text": "Test: It says new.", "status": "done"},
                            {"text": "Test: The rest stays.", "status": "done"}
                        ]}),
                    },
                ]),
                Scripted::text("Changed, with tests."),
            ],
        );
        let worker_thinking = Scripted::new("worker-thinking", vec![]);
        let (events, mut rx) = mpsc::channel(256);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker_thinking,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Always,
            review_rounds: 2,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("say new", Mode::Plan, &mut messages)
            .await
            .unwrap();
        drop(tandem);

        let brief = reviewer.seen.lock().unwrap()[0].clone();
        assert!(
            brief.contains("## New tests, run without the change")
                && brief.contains("`check.test.js:1` \"says new\": fails without the change")
                && brief.contains("\"keeps the rest\": passes without the change"),
            "{brief}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("code.txt")).unwrap(),
            "new\n",
            "the run was in a copy"
        );
        let report = out.report.expect("a report");
        assert!(report.complete, "{:?}", report.problems);
        assert_eq!(report.done_when[0].without, Some(Without::Fails));
        assert_eq!(report.done_when[1].without, Some(Without::Passes));
        assert_eq!(report.headline(), "plan complete · 1 thing to look at");
        assert_eq!(report.review, "approved");
        let mut finished = false;
        while let Ok(evt) = rx.try_recv() {
            finished |= matches!(evt, AgentEvent::Finished { .. });
        }
        assert!(finished, "the card was sent to the screen");
    }

    #[test]
    fn earlier_asks_not_made_again_are_settled() {
        let list = vec![
            Todo::pending("step"),
            Todo::pending("Fix: a"),
            Todo::pending("Fix: b"),
            Todo::pending("Fix: from an earlier task"),
        ];
        let asked = ["Fix: a".to_string(), "Fix: b".to_string()];
        let list = settle(list, &asked, &["Fix: b".into(), "Fix: c".into()]);
        let shown: Vec<(&str, bool)> = list
            .iter()
            .map(|t| (t.text.as_str(), t.is_open()))
            .collect();
        assert_eq!(
            shown,
            [
                ("step", true),
                ("Fix: a", false),
                ("Fix: b", true),
                ("Fix: from an earlier task", true),
                ("Fix: c", true)
            ]
        );
    }

    /// One edit through `Mode::Go`, reviewed by `reviewer`; the outcome and
    /// how many fix rounds the worker was sent on.
    async fn reviewed(reviewer: Vec<Reply>) -> (TandemOutcome, usize, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![]);
        let reviewer = Scripted::new("reviewer", reviewer);
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::calls(vec![edit("a.txt", "one", "two")]),
                Scripted::text("Changed."),
            ],
        );
        let worker_thinking = Scripted::new("worker-thinking", vec![]);
        let (events, _rx) = mpsc::channel(256);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker_thinking,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Never,
            review_rounds: 2,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("make it two", Mode::Go, &mut messages)
            .await
            .unwrap();
        let fixes = worker_thinking.seen.lock().unwrap().len();
        let asked = reviewer.seen.lock().unwrap().clone();
        (out, fixes, asked)
    }

    #[tokio::test]
    async fn a_review_without_a_verdict_is_asked_again_then_called_inconclusive() {
        // Asked again, it answers: that verdict stands.
        let (out, fixes, asked) = reviewed(vec![
            Scripted::text("<tool_call>\n<function=search>\n<parameter=pattern>\nx\n</parameter>\n</function>\n</tool_call>"),
            Scripted::text("VERDICT: APPROVE\nNothing to add"),
        ])
        .await;
        assert_eq!(out.reviews, vec![(true, "Nothing to add".to_string())]);
        assert!(!out.review_inconclusive);
        assert_eq!(fixes, 0);
        assert_eq!(asked[1], prompts::VERDICT_ONLY);

        // Twice without one: not approved, and the worker is not sent after a guess.
        let (out, fixes, _) = reviewed(vec![
            Scripted::text("Looks fine, mostly."),
            Scripted::text("Still looking."),
        ])
        .await;
        assert!(out.review_inconclusive);
        assert_eq!(out.reviews.len(), 1);
        assert!(
            !out.reviews[0].0
                && out.reviews[0].1.contains("not reviewed")
                && out.reviews[0].1.contains("Still looking.")
        );
        assert_eq!(fixes, 0);
    }

    #[tokio::test]
    async fn checks_that_never_pass_are_tried_a_few_times_then_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = checked_ctx(dir.path(), "echo broken; false");
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![]);
        let reviewer = Scripted::new(
            "reviewer",
            vec![Scripted::text("VERDICT: FIX\n1. `a.txt`: the checks fail")],
        );
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::calls(vec![edit("a.txt", "one", "two")]),
                Scripted::text("Changed."),
            ],
        );
        let worker_thinking = Scripted::new(
            "worker-thinking",
            (0..CHECK_FIXES)
                .map(|_| Scripted::text("Could not fix it."))
                .collect(),
        );
        let (events, _rx) = mpsc::channel(256);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &reviewer,
            worker: &worker,
            worker_thinking: &worker_thinking,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Never,
            review_rounds: 1,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("make it two", Mode::Go, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.checks, Some(false));
        assert!(out.fixes_pending);
        assert_eq!(worker_thinking.seen.lock().unwrap().len(), CHECK_FIXES);
        let review = reviewer.seen.lock().unwrap().clone();
        assert!(
            review[0].contains("`echo broken; false` FAILED\nbroken"),
            "{}",
            review[0]
        );
    }

    #[test]
    fn a_plan_splits_into_the_persons_part_and_the_workers() {
        let plan = shaped("1. `a.rs`: x\n\n## Verify\ncargo test");
        let (mine, worker) = split_plan(&plan);
        assert!(
            mine.starts_with("## What you asked for") && mine.ends_with("It reads as asked."),
            "{mine}"
        );
        assert_eq!(
            worker,
            Some("## Steps\n1. `a.rs`: x\n\n## Verify\ncargo test")
        );
        assert!(missing_sections(&plan).is_empty());
        assert_eq!(
            split_plan("## Plan\n1. old shape"),
            ("## Plan\n1. old shape", None)
        );
        assert_eq!(
            missing_sections("## What you asked for\nx\n\n## Steps\n1. y"),
            [
                "## What will change for you",
                "## Choices to confirm",
                "## How it fits together",
                "## Done when"
            ]
        );
        assert_eq!(plan_items(&plan), ["`a.rs`: x"]);
    }

    #[tokio::test]
    async fn a_plan_missing_the_persons_part_is_asked_for_again_and_every_plan_waits() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin();
        let planner = Scripted::new(
            "planner",
            vec![
                Scripted::text("## Plan\n1. `a.rs`: do it"),
                Scripted::text(&shaped("1. `a.rs`: do it")),
            ],
        );
        let worker = Scripted::new("worker", vec![Scripted::text("PLAN")]);
        let (events, _rx) = mpsc::channel(64);
        // Triage asked for the plan (no /plan), and it still waits for a word.
        let approver = PlanScript(Mutex::new(vec![PlanDecision::Stop]));
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &planner,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &approver,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Auto,
            review_rounds: 1,
            confirm: Confirm::Asked,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("do it", Mode::Auto, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.answer, "Stopped at the plan.");
        assert_eq!(out.plan, Some(shaped("1. `a.rs`: do it")));
        let seen = planner.seen.lock().unwrap().clone();
        assert!(
            seen[1].starts_with(
                "Your plan left out ## What you asked for, ## What will change for you"
            ),
            "{}",
            seen[1]
        );
    }

    #[test]
    fn the_checklist_comes_from_the_tasks_or_else_the_plan() {
        assert_eq!(
            plan_items("## Plan\n1. `a.rs`: x\n2. `b.rs`: y\n\n## Verify\ncargo test"),
            ["`a.rs`: x", "`b.rs`: y"]
        );
        assert_eq!(
            plan_items("## Plan\n1. big\n\n## Tasks\n1. First.\n2. Second."),
            ["First.", "Second."]
        );
        assert!(plan_items("Just look at it.").is_empty());
        assert_eq!(
            plan_items(
                "## Plan\n1. `a.rs`: x\n\n## Done when\n1. Saying hi gets hi back.\n\n## Verify\nt"
            ),
            ["`a.rs`: x", "Test: Saying hi gets hi back."]
        );
    }

    #[tokio::test]
    async fn open_todos_from_before_reach_the_planner_and_a_question_is_not_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        *ctx.todos.lock().unwrap() = vec![
            Todo {
                text: "Remove the widget".into(),
                status: Status::Done,
            },
            Todo::pending("Teach the coach"),
        ];
        let registry = Registry::builtin();
        let planner = Scripted::new("planner", vec![]);
        // Asked about something else, the worker answers once: the list it
        // did not touch is not a reason to send it back.
        let worker = Scripted::new("worker", vec![Scripted::text("GO"), Scripted::text("No.")]);
        let (events, _rx) = mpsc::channel(64);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &planner,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Auto,
            review_rounds: 1,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("did you restart it?", Mode::Auto, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.answer, "No.");
        let seen = worker.seen.lock().unwrap().clone();
        assert!(
            seen[0].contains("## Open todos\nAn earlier task")
                && seen[0].contains("☐ Teach the coach"),
            "triage: {}",
            seen[0]
        );
        assert!(
            seen[1].contains("☒ Remove the widget\n☐ Teach the coach"),
            "worker: {}",
            seen[1]
        );
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn steps_come_from_the_tasks_section() {
        let plan = "## Plan\n1. a.rs: x\n\n## Verify\ncargo test\n\n## Tasks\n1. Add the type.\n2) Wire it in.\n\n## Watch out\nNothing";
        assert_eq!(steps_of(plan), ["Add the type.", "Wire it in."]);
        assert!(steps_of("## Plan\n1. a.rs: x").is_empty());
    }

    #[tokio::test]
    async fn a_plan_with_tasks_runs_the_worker_once_per_task() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin();
        let planner = Scripted::new(
            "planner",
            vec![Scripted::text(&shaped(
                "1. do things\n\n## Verify\ntrue\n\n## Tasks\n1. First piece.\n2. Second piece.\n3. Third piece.",
            ))],
        );
        let worker = Scripted::new(
            "worker",
            vec![
                Scripted::text("Did one."),
                Scripted::text("Did two."),
                Scripted::text("Did three."),
            ],
        );
        let (events, mut rx) = mpsc::channel(64);
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &planner,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &Yes,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Always,
            review_rounds: 1,
            confirm: Confirm::Never,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("build it all", Mode::Plan, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.answer, "Did three.");
        let seen = worker.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[0].contains("build it all") && seen[0].contains("## Step 1 of 3"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("## Step 2 of 3\nSecond piece."),
            "{}",
            seen[1]
        );
        // The steps are not tasks of their own in the session's eyes.
        assert_eq!(
            exchanges(&messages[1..])
                .iter()
                .map(|(t, _)| t.as_str())
                .collect::<Vec<_>>(),
            ["build it all"]
        );
        drop(tandem);
        let mut steps = 0;
        while let Some(evt) = rx.recv().await {
            if matches!(evt, AgentEvent::Step { .. }) {
                steps += 1;
            }
        }
        assert_eq!(steps, 3);
    }

    #[test]
    fn snapshot_names_an_empty_folder_and_a_stack() {
        let dir = tempfile::tempdir().unwrap();
        let s = snapshot(&Ctx::new(dir.path().to_path_buf()));
        assert!(s.contains("The folder is empty"), "{s}");
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/components")).unwrap();
        std::fs::write(dir.path().join("src/App.tsx"), "").unwrap();
        std::fs::write(dir.path().join("src/components/Monitors.test.tsx"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        let s = snapshot(&Ctx::new(dir.path().to_path_buf()));
        assert!(
            s.contains("Stack: Node") && s.contains("Top level: package.json src/"),
            "{s}"
        );
        assert!(
            s.contains("src/: App.tsx components/\nsrc/components/: Monitors.test.tsx\n")
                && !s.contains("node_modules/:"),
            "{s}"
        );
    }

    #[test]
    fn reads_verdicts() {
        assert_eq!(
            verdict("VERDICT: APPROVE\nNothing to add"),
            (Some(true), "Nothing to add".into())
        );
        assert_eq!(
            verdict("  **VERDICT:** FIX\n1. a\n2. b"),
            (Some(false), "1. a\n2. b".into())
        );
        // No verdict line is no verdict, not approval.
        assert_eq!(
            verdict("Looks fine to me."),
            (None, "Looks fine to me.".into())
        );
        // As the last run's reviewer answered: tool calls written out, nothing else.
        assert_eq!(
            verdict(
                "<tool_call>\n<function=search>\n<parameter=pattern>\nkindView\n</parameter>\n</function>\n</tool_call>"
            ),
            (None, String::new())
        );
        assert_eq!(
            verdict(
                "Checked.\n<tool_call>\n<function=search>\n</function>\n</tool_call>\nVERDICT: APPROVE\nFine."
            ),
            (Some(true), "Checked.\n\nFine.".into())
        );
    }

    #[tokio::test]
    async fn a_file_removed_through_bash_reaches_the_reviewer() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("old.tsx"), "gone soon\n").unwrap();
        std::fs::write(root.join("dirty.ts"), "a\n").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--no-verify",
            "-qm",
            "test: start",
        ]);
        std::fs::write(root.join("dirty.ts"), "b\n").unwrap();
        let before = git_changes(root).await;
        std::fs::remove_file(root.join("old.tsx")).unwrap();
        let changed = changed_since(&before, &git_changes(root).await);
        assert_eq!(
            changed,
            ["old.tsx"],
            "dirty before the task is not the task's"
        );
        assert!(diff_of(root, &changed).await.contains("-gone soon"));
    }

    #[tokio::test]
    async fn diff_falls_back_to_whole_files_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("n.txt"), "new\n").unwrap();
        let d = diff_of(dir.path(), &["n.txt".into(), "missing.txt".into()]).await;
        assert_eq!(d, "--- new or untracked file: n.txt\nnew\n\n");
    }
    /// Answers plan reviews from a script.
    struct PlanScript(Mutex<Vec<PlanDecision>>);

    #[async_trait]
    impl Approver for PlanScript {
        async fn ask(&self, _what: &str) -> super::super::Decision {
            super::super::Decision::Always
        }
        async fn review_plan(&self, _plan: &str) -> PlanDecision {
            self.0.lock().unwrap().remove(0)
        }
    }

    #[tokio::test]
    async fn plan_mode_revises_on_notes_and_stops_before_work() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(dir.path().to_path_buf());
        let registry = Registry::builtin();
        let planner = Scripted::new(
            "planner",
            vec![
                Scripted::text(&shaped("1. first draft")),
                Scripted::text(&shaped("1. second draft")),
            ],
        );
        let worker = Scripted::new("worker", vec![]); // never asked
        let (events, mut rx) = mpsc::channel(64);
        let approver = PlanScript(Mutex::new(vec![
            PlanDecision::Revise("skip the tests".into()),
            PlanDecision::Stop,
        ]));
        let mut tandem = Tandem {
            planner: &planner,
            reviewer: &planner,
            worker: &worker,
            worker_thinking: &worker,
            registry: &registry,
            planner_registry: &registry,
            ctx: &ctx,
            approver: &approver,
            events,
            cancel: CancellationToken::new(),
            allowed: HashSet::new(),
            when: PlanWhen::Auto,
            review_rounds: 1,
            confirm: Confirm::Asked,
            lead: String::new(),
            window: crate::config::DEFAULT_WINDOW,
            digest: None,
        };
        let mut messages = vec![Message::System("sys".into())];
        let out = tandem
            .run("tidy up", Mode::Plan, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.answer, "Stopped at the plan.");
        assert_eq!(out.plan, Some(shaped("1. second draft")));
        let seen = planner.seen.lock().unwrap().clone();
        assert!(seen[1].contains("skip the tests"), "{}", seen[1]);
        assert_eq!(
            messages.len(),
            1,
            "the worker's conversation was not touched"
        );
        drop(tandem);
        let mut plans = 0;
        while let Some(evt) = rx.recv().await {
            if matches!(evt, AgentEvent::Plan { .. }) {
                plans += 1;
            }
        }
        assert_eq!(plans, 2);
    }
}
