//! `~/.config/weft/config.toml`: where LM Studio is, and which model plays which role.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::hooks::Hook;
use crate::lm::Effort;
use crate::tools::Rules;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub lm_studio: LmStudio,
    pub planner: Planner,
    pub worker: Worker,
    #[serde(default)]
    pub scout: Scout,
    #[serde(default)]
    pub compact: Compact,
    /// Tool calls that run without asking.
    #[serde(default)]
    pub approve: Rules,
    /// Your commands at set moments; see `hooks`.
    #[serde(default)]
    pub hooks: Vec<Hook>,
    #[serde(default)]
    pub vision: Vision,
    /// Settings for one folder: `[[project]]`, matched by `path`.
    #[serde(default, rename = "project")]
    pub projects: Vec<Project>,
}

/// What weft knows about one project without anything in its repository.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// The project's folder; `~/` is the home folder.
    pub path: String,
    /// The checks weft runs itself once the worker is done, a shell line:
    /// `npx tsc --noEmit && npx vitest run`. A failure goes back to the worker.
    #[serde(default)]
    pub verify: Option<String>,
    /// Runs some test files, `{files}` standing for them:
    /// `npx vitest run --reporter=verbose {files}`. weft runs the tests a task
    /// added with the code put back as it was, to see that they fail without
    /// the change. Guessed from the project's files when not given.
    #[serde(default)]
    pub test: Option<String>,
    /// Puts the project's changes live, a shell line: `npm run deploy`. The
    /// worker is told to use it rather than stop and start servers itself,
    /// and an answer that says the change is live without it is flagged.
    #[serde(default)]
    pub deploy: Option<String>,
    /// Starts a dev server for the worker to look at, `{port}` standing for
    /// a free port weft picks: `npx vite --port {port} --strictPort`.
    #[serde(default)]
    pub serve: Option<String>,
    /// Seconds the checks may take before they count as failed.
    #[serde(default = "default_verify_timeout")]
    pub verify_timeout_s: u64,
    /// Standing instructions, shown to every model like a `WEFT.md`.
    #[serde(default)]
    pub notes: Option<String>,
}

fn default_verify_timeout() -> u64 {
    300
}

/// The model that looks at images for `see_image`: a vision model on the
/// chat route. The planner's by default; your models are all vision models.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vision {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "Effort::low")]
    pub effort: Effort,
}

impl Default for Vision {
    fn default() -> Self {
        Self {
            model: None,
            effort: Effort::Low,
        }
    }
}

/// When the worker's conversation is folded into a handover note.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compact {
    /// Compact after a task once the worker's context reaches this many
    /// tokens. 0 never compacts on its own; `/compact` still works.
    #[serde(default = "default_compact_at")]
    pub at: u32,
    /// The worker's context window, in tokens: what the model was loaded
    /// with. Inside a task, old tool results are dropped as the window fills.
    #[serde(default = "default_window")]
    pub window: u32,
}

impl Default for Compact {
    fn default() -> Self {
        Self {
            at: default_compact_at(),
            window: default_window(),
        }
    }
}

fn default_compact_at() -> u32 {
    48_000
}

/// LM Studio's usual load setting here.
pub const DEFAULT_WINDOW: u32 = 65_536;

fn default_window() -> u32 {
    DEFAULT_WINDOW
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LmStudio {
    /// `http://127.0.0.1:1234`, no trailing slash.
    #[serde(default = "default_url")]
    pub url: String,
    /// The API token, or the `LM_API_TOKEN` environment variable when missing.
    #[serde(default)]
    pub api_token: Option<String>,
}

/// The model that plans and reviews: it answers on the OpenAI-style chat route,
/// which lets LM Studio apply its reasoning-effort levels.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Planner {
    pub model: String,
    #[serde(default = "Effort::medium")]
    pub plan_effort: Effort,
    /// Low by default: a review is one read of a diff, and at medium the 27B
    /// spent over a minute approving a two-line change.
    #[serde(default = "Effort::low")]
    pub review_effort: Effort,
    /// Plan before every task, never, or let a quick triage decide.
    #[serde(default)]
    pub when: PlanWhen,
    /// How many reviews a task gets. Every `FIX` but the last sends the
    /// worker back, so 2 is one fix pass. 0 turns review off.
    #[serde(default = "default_review_rounds")]
    pub review_rounds: u8,
    /// When the plan waits for your go-ahead before the worker starts.
    #[serde(default)]
    pub confirm: Confirm,
}

/// `asked`: every plan pauses for a word (all-auto answers it at once).
/// `always`: the same. `never`: the worker always starts at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confirm {
    #[default]
    Asked,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanWhen {
    #[default]
    Auto,
    Always,
    Never,
}

fn default_review_rounds() -> u8 {
    2
}

/// The model that runs the tool loop: prompted on the raw completions route,
/// so its thinking can be switched off.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub model: String,
    /// Think by default? Off: the plan carries the reasoning.
    #[serde(default)]
    pub thinking: bool,
}

/// The model that answers the scout's questions: the worker's by default,
/// thinking off, because finding a file wants speed, not deep thought.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scout {
    /// Default: the worker's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Set this for a model that honours `reasoning_effort` (the chat route).
    /// Left out, the scout's model runs on the raw route with thinking off,
    /// like the worker.
    #[serde(default)]
    pub effort: Option<Effort>,
}

fn default_url() -> String {
    "http://127.0.0.1:1234".to_string()
}

impl Config {
    /// The `[[project]]` for `root`, if one names it.
    pub fn project_for(&self, root: &Path) -> Option<&Project> {
        let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let root = real(root);
        self.projects.iter().find(|p| {
            let path = match (p.path.strip_prefix("~/"), std::env::home_dir()) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => PathBuf::from(&p.path),
            };
            real(&path) == root
        })
    }

    pub fn path() -> Result<PathBuf> {
        let home = std::env::home_dir().context("no home folder")?;
        Ok(home.join(".config/weft/config.toml"))
    }

    pub fn load(file: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        // Rust: `Option::or_else` fills a None from a closure; `.ok()` turns the
        // env lookup's Result into an Option so the two fit together.
        config.lm_studio.api_token = config
            .lm_studio
            .api_token
            .or_else(|| std::env::var("LM_API_TOKEN").ok());
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_defaults() {
        let text = r#"
[lm_studio]
[planner]
model = "qwen/qwen3.8-27b"
[worker]
model = "qwen3.5-122b-a10b"
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.lm_studio.url, "http://127.0.0.1:1234");
        assert_eq!(config.planner.plan_effort, Effort::Medium);
        assert_eq!(config.planner.review_effort, Effort::Low);
        assert_eq!(config.planner.when, PlanWhen::Auto);
        assert_eq!(config.planner.review_rounds, 2);
        assert_eq!(config.planner.confirm, Confirm::Asked);
        assert!(!config.worker.thinking);
        assert_eq!(config.scout.model, None);
        assert_eq!(config.scout.effort, None);
        assert_eq!(config.compact.at, 48_000);
        assert_eq!(config.compact.window, 65_536);
        assert!(config.approve.commands.is_empty());
        assert!(config.hooks.is_empty());
        assert_eq!(config.vision.model, None);
        assert_eq!(config.vision.effort, Effort::Low);
    }

    #[test]
    fn a_project_is_found_by_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            r#"
[lm_studio]
[planner]
model = "p"
[worker]
model = "w"
[[project]]
path = "{}"
verify = "npm test"
deploy = "npm run deploy"
serve = "npx vite --port {{port}}"
notes = "Commits are one line."
"#,
            dir.path().display()
        );
        let config: Config = toml::from_str(&text).unwrap();
        let p = config.project_for(dir.path()).unwrap();
        assert_eq!(p.verify.as_deref(), Some("npm test"));
        assert_eq!(p.deploy.as_deref(), Some("npm run deploy"));
        assert_eq!(p.serve.as_deref(), Some("npx vite --port {port}"));
        assert_eq!(p.verify_timeout_s, 300);
        assert!(config.project_for(Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn a_misspelt_key_is_an_error_not_a_silent_default() {
        let text = r#"
[lm_studio]
[planner]
model = "p"
review_efort = "low"
[worker]
model = "w"
"#;
        let err = toml::from_str::<Config>(text).unwrap_err().to_string();
        assert!(err.contains("review_efort"), "{err}");
    }
}
