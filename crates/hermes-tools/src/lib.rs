//! Tool trait, registry, and built-in tools

pub mod file_read;
pub mod path_utils;
pub mod registry;
pub mod terminal;
pub use registry::{ToolRegistration, ToolRegistry};
