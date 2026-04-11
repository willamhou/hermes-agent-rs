//! Agent loop, iteration budget, context compression

pub mod budget;
pub mod loop_runner;
pub mod parallel;
pub mod token_counter;

pub use loop_runner::{Agent, AgentConfig};
