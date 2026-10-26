//! Which model plays which role, and how each is reached: one place that
//! turns the config into model handles and the tools that need one.

use std::path::Path;

use crate::agent::Model;
use crate::agent::scout::Scout;
use crate::config::Config;
use crate::lm::chat::ChatModel;
use crate::lm::raw::RawModel;
use crate::lm::{Effort, Endpoint};
use crate::tools::Tool;
use crate::tools::image::SeeImage;
use crate::tools::memory::{self, Remember};

#[derive(Debug, Clone)]
pub struct Models {
    url: String,
    api_token: Option<String>,
    pub planner: String,
    pub plan_effort: Effort,
    pub review_effort: Effort,
    pub worker: String,
    /// The scout's model: the worker's unless the config names another.
    pub scout: String,
    /// On the chat route at this effort; `None` is the raw route, thinking off.
    pub scout_effort: Option<Effort>,
    /// The vision model for `see_image`: the planner's unless named.
    pub vision: String,
    pub vision_effort: Effort,
}

impl Models {
    pub fn from(config: &Config) -> Self {
        Self {
            url: config.lm_studio.url.clone(),
            api_token: config.lm_studio.api_token.clone(),
            planner: config.planner.model.clone(),
            plan_effort: config.planner.plan_effort,
            review_effort: config.planner.review_effort,
            worker: config.worker.model.clone(),
            scout: config
                .scout
                .model
                .clone()
                .unwrap_or_else(|| config.worker.model.clone()),
            scout_effort: config.scout.effort,
            vision: config
                .vision
                .model
                .clone()
                .unwrap_or_else(|| config.planner.model.clone()),
            vision_effort: config.vision.effort,
        }
    }

    pub fn endpoint(&self, model: &str) -> Endpoint {
        Endpoint {
            url: self.url.clone(),
            api_token: self.api_token.clone(),
            model: model.to_string(),
        }
    }

    /// The planner at plan effort.
    pub fn planner(&self) -> ChatModel {
        ChatModel::new(self.endpoint(&self.planner), Some(self.plan_effort))
    }

    /// The same model at review effort.
    pub fn reviewer(&self) -> ChatModel {
        ChatModel::new(self.endpoint(&self.planner), Some(self.review_effort))
    }

    /// The worker on the raw route, thinking on or off.
    pub fn worker(&self, thinking: bool) -> RawModel {
        RawModel::new(self.endpoint(&self.worker), thinking)
    }

    /// The scout's model: the raw route with thinking off, unless the config
    /// gave it an effort, which only the chat route carries. A dense model at
    /// low effort took close to three minutes on one question; a small model
    /// on the raw route takes seconds.
    pub fn scout(&self) -> Box<dyn Model> {
        match self.scout_effort {
            Some(effort) => Box::new(ChatModel::new(self.endpoint(&self.scout), Some(effort))),
            None => Box::new(RawModel::new(self.endpoint(&self.scout), false)),
        }
    }

    /// The tools that need a model or a place of their own, built fresh: the
    /// scout, a look at images, and memory for `root`.
    pub fn extra_tools(&self, root: &Path) -> Vec<Box<dyn Tool>> {
        let mut out: Vec<Box<dyn Tool>> = vec![
            Box::new(Scout::new(self.scout())),
            Box::new(SeeImage {
                endpoint: self.endpoint(&self.vision),
                effort: Some(self.vision_effort),
            }),
        ];
        if let Some(file) = memory::file_for(root) {
            out.push(Box::new(Remember { file }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fall_back_to_the_worker_and_the_planner() {
        let config: Config = toml::from_str(
            r#"
[lm_studio]
[planner]
model = "p"
[worker]
model = "w"
"#,
        )
        .unwrap();
        let m = Models::from(&config);
        assert_eq!((m.scout.as_str(), m.vision.as_str()), ("w", "p"));
        assert_eq!(m.scout_effort, None);
        assert_eq!(m.endpoint("x").model, "x");
        assert_eq!(m.worker(false).endpoint.model, "w");
        let names: Vec<String> = m
            .extra_tools(Path::new("/tmp/p"))
            .iter()
            .map(|t| t.spec().name)
            .collect();
        assert!(names.starts_with(&["scout".to_string(), "see_image".to_string()]));
    }
}
