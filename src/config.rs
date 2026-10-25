//! `~/.config/weft/config.toml`: where LM Studio is, and which model plays which role.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::hooks::Hook;
use crate::lm::Effort;
use crate::tools::Rules;

#[derive(Debug, Clone, Deserialize)]
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
}

/// When the worker's conversation is folded into a handover note.
#[derive(Debug, Clone, Deserialize)]
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
    /// How many times the reviewer may send the worker back. 0 turns review off.
    #[serde(default = "default_review_rounds")]
    pub review_rounds: u8,
    /// When the plan waits for your go-ahead before the worker starts.
    #[serde(default)]
    pub confirm: Confirm,
}

/// `asked`: only a `/plan` task pauses on its plan. `always`: every plan
/// does. `never`: the worker always starts at once.
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
    1
}

/// The model that runs the tool loop: prompted on the raw completions route,
/// so its thinking can be switched off.
#[derive(Debug, Clone, Deserialize)]
pub struct Worker {
    pub model: String,
    /// Think by default? Off: the plan carries the reasoning.
    #[serde(default)]
    pub thinking: bool,
}

/// The model that answers the scout's questions: the worker's by default,
/// thinking off, because finding a file wants speed, not deep thought.
#[derive(Debug, Clone, Default, Deserialize)]
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
        assert_eq!(config.planner.review_rounds, 1);
        assert_eq!(config.planner.confirm, Confirm::Asked);
        assert!(!config.worker.thinking);
        assert_eq!(config.scout.model, None);
        assert_eq!(config.scout.effort, None);
        assert_eq!(config.compact.at, 48_000);
        assert_eq!(config.compact.window, 65_536);
        assert!(config.approve.commands.is_empty());
        assert!(config.hooks.is_empty());
    }
}
