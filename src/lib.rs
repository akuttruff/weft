//! weft: a coding agent for the terminal that runs two local models in tandem.
//!
//! The planner (a dense model, asked to think hard) writes the plan and reviews
//! the result; the worker (a fast MoE model, thinking off) runs the tool loop.
//! Both are served by LM Studio; see [`lm`] for the two routes.

pub mod agent;
pub mod checkpoint;
pub mod config;
pub mod lm;
pub mod mcp;
pub mod session;
pub mod tools;
pub mod transcript;
pub mod ui;
