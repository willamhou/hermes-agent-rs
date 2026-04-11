//! Agent loop, iteration budget, context compression

pub mod budget;
pub mod cache_manager;
pub mod compressor;
pub mod loop_runner;
pub mod parallel;
pub mod token_counter;

pub use loop_runner::{Agent, AgentConfig};
