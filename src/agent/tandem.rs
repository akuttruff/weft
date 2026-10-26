//! The two models in tandem: the planner looks and writes a plan, the worker
//! carries it out, the reviewer reads the diff and may send the worker back.

use std::collections::HashSet;
use std::path::Path;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::convo::{exchanges, task_message};
use super::worker::{Outcome, Worker, WorkerError};
use super::{AgentEvent, Approver, Events, Model, PlanDecision, Role, Yes, prompts};
use crate::config::{Confirm, PlanWhen};
use crate::lm::{Message, Usage};
use crate::tools::{Ctx, Registry};

/// Tool rounds the planner gets to look around before it must write the plan.
const PLANNER_ROUNDS: usize = 8;
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
        // The planner, the triage and the reviewer do not see the worker's
        // conversation, so "now add a test for that" needs the earlier exchange.
        let earlier = earlier(messages);
        let project = snapshot(&self.ctx.root);

        // 1. Plan, if this task warrants one.
        let context = format!("{project}{}", earlier.as_deref().unwrap_or(""));
        let plan = match self.should_plan(task, Some(&context), mode).await? {
            false => None,
            true => {
                self.phase("planning").await;
                let mut planner = self.looker(
                    self.planner,
                    self.planner_registry,
                    Role::Planner,
                    PLANNER_ROUNDS,
                );
                let mut convo = vec![
                    Message::System(prompts::PLANNER.into()),
                    Message::User(with_earlier(task, Some(&context))),
                ];
                let mut plan = planner.run(&mut convo).await?;
                out.planner_usage = add(out.planner_usage, plan.usage);
                self.emit(AgentEvent::Plan {
                    text: plan.answer.clone(),
                })
                .await;

                // Plan mode: the user reads the plan first and may send it
                // back with notes a few times, or stop here.
                let pause = match self.confirm {
                    Confirm::Always => true,
                    Confirm::Never => false,
                    Confirm::Asked => mode == Mode::Plan,
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

        // 2. Work.
        self.phase("working").await;
        // The worker sees the project note on its first task; after that the
        // conversation carries it.
        let fresh = !messages.iter().any(|m| matches!(m, Message::User(_)));
        let lead = if fresh {
            project.clone()
        } else {
            self.lead.clone()
        };
        messages.push(Message::User(task_message(&lead, task, plan.as_deref())));
        // A plan with a `## Tasks` section runs as rounds: the worker does
        // one, answers, and gets the next, so each starts with a fresh look
        // at the code and the window is trimmed between them.
        let steps = plan.as_deref().map(steps_of).unwrap_or_default();
        if steps.len() >= 2 {
            for (i, step) in steps.iter().enumerate() {
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
        } else {
            let done = self.work(self.worker, messages).await?;
            out.take(&done);
        }

        // 3. Review what was changed, and send the worker back if asked.
        // Helpers' edits count too; the checkpoint saw every file the tools changed.
        if let Some(cp) = &self.ctx.checkpoint {
            for f in cp.files() {
                if !out.touched.contains(&f) {
                    out.touched.push(f);
                }
            }
        }
        let mut round = 0;
        while !out.touched.is_empty() && round < self.review_rounds {
            round += 1;
            self.phase("reviewing").await;
            let diff = diff_of(&self.ctx.root, &out.touched).await;
            let brief = review_brief(
                &with_earlier(task, Some(&context)),
                plan.as_deref(),
                &out.answer,
                &diff,
            );
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
                // A quick, silent word from the worker: its events go nowhere.
                let (quiet, _keep) = mpsc::channel(8);
                let empty = Registry::empty();
                let mut triage = Worker::new(self.worker, &empty, self.ctx, &Yes, quiet)
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
    let Some((_, after)) = plan.split_once("## Tasks") else {
        return Vec::new();
    };
    let section = after.split("\n## ").next().unwrap_or("");
    section
        .lines()
        .map(str::trim)
        .filter_map(|l| {
            let rest = l.trim_start_matches(|c: char| c.is_ascii_digit());
            (rest.len() < l.len() && (rest.starts_with('.') || rest.starts_with(')')))
                .then(|| rest[1..].trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
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
pub fn snapshot(root: &Path) -> String {
    let global = std::env::home_dir().map(|h| h.join(".config/weft/WEFT.md"));
    let kept = crate::tools::memory::file_for(root);
    format!(
        "{}{}",
        folder_note(root),
        notes(root, global.as_deref(), kept.as_deref())
    )
}

/// How much of a notes file the models are shown, in characters.
const NOTES_CHARS: usize = 8_000;

/// The user's standing instructions: `~/.config/weft/WEFT.md` for every
/// project and `WEFT.md` in the folder for this one, then what weft itself
/// kept with `remember`, as a `## Project notes` section. Empty when there
/// are none.
pub fn notes(root: &Path, global: Option<&Path>, kept: Option<&Path>) -> String {
    let mut out = String::new();
    let files = [
        ("for every project", global.map(Path::to_path_buf)),
        ("for this project", Some(root.join("WEFT.md"))),
        ("weft kept from earlier tasks", kept.map(Path::to_path_buf)),
    ];
    for (label, path) in files {
        let Some(path) = path else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "### Notes {label} ({})\n{}\n\n",
            path.display(),
            clip(text.trim(), NOTES_CHARS)
        ));
    }
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
        let ctx = Ctx::new(dir.path().to_path_buf());
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
        assert!(
            seen[0].starts_with("## Project\n") && seen[0].ends_with("## Task\nmake it two"),
            "triage saw the project note and the task: {}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("## Project\n")
                && seen[1].contains("make it two\n\n")
                && seen[1].contains("## Plan"),
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
        let planner = Scripted::new("planner", vec![Scripted::text("## Plan\n1. add a test")]);
        let reviewer = Scripted::new("reviewer", vec![]);
        let worker = Scripted::new("worker", vec![Scripted::text("Added the test.")]);
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
        assert_eq!(notes(dir.path(), None, None), "");
        std::fs::write(
            dir.path().join("WEFT.md"),
            "Run `cargo test` before answering.\n",
        )
        .unwrap();
        let global = dir.path().join("global.md");
        std::fs::write(&global, "One-line commits.").unwrap();
        let kept = dir.path().join("kept.md");
        std::fs::write(&kept, "- Tests take a minute.").unwrap();
        let text = notes(dir.path(), Some(&global), Some(&kept));
        assert!(
            text.contains("weft kept from earlier tasks") && text.contains("Tests take a minute.")
        );
        assert!(text.starts_with("## Project notes\n"), "{text}");
        let every = text.find("for every project").unwrap();
        let this = text.find("for this project").unwrap();
        assert!(every < this && text.contains("One-line commits.") && text.contains("cargo test"));
        let s = snapshot(dir.path());
        assert!(
            s.starts_with("## Project\n") && s.contains("## Project notes"),
            "{s}"
        );
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
            vec![Scripted::text(
                "## Plan\n1. do things\n\n## Verify\ntrue\n\n## Tasks\n1. First piece.\n2. Second piece.\n3. Third piece.",
            )],
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
        let s = snapshot(dir.path());
        assert!(s.contains("The folder is empty"), "{s}");
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/components")).unwrap();
        std::fs::write(dir.path().join("src/App.tsx"), "").unwrap();
        std::fs::write(dir.path().join("src/components/Monitors.test.tsx"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        let s = snapshot(dir.path());
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
                Scripted::text("## Plan\n1. first draft"),
                Scripted::text("## Plan\n1. second draft"),
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
        assert_eq!(out.plan.as_deref(), Some("## Plan\n1. second draft"));
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
