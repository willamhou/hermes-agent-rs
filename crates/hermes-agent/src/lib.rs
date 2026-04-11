//! Agent loop, iteration budget, context compression

pub mod budget;
pub mod loop_runner;
pub mod parallel;

pub use loop_runner::{Agent, AgentConfig};
