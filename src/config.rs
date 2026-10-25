//! `~/.config/weft/config.toml`: where LM Studio is, and which model plays which role.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::lm::Effort;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub lm_studio: LmStudio,
    pub planner: Planner,
    pub worker: Worker,
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
    #[serde(default = "Effort::xhigh")]
    pub plan_effort: Effort,
    #[serde(default = "Effort::medium")]
    pub review_effort: Effort,
    /// Plan before every task, never, or let a quick triage decide.
    #[serde(default)]
    pub when: PlanWhen,
    /// How many times the reviewer may send the worker back. 0 turns review off.
    #[serde(default = "default_review_rounds")]
    pub review_rounds: u8,
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
        assert_eq!(config.planner.plan_effort, Effort::Xhigh);
        assert_eq!(config.planner.review_effort, Effort::Medium);
        assert_eq!(config.planner.when, PlanWhen::Auto);
        assert_eq!(config.planner.review_rounds, 1);
        assert!(!config.worker.thinking);
    }
}
