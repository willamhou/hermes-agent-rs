//! Tool trait, registry, and built-in tools

mod approval_key;
pub mod browser;
pub mod execute_code;
pub mod file_read;
pub mod file_search;
pub mod file_write;
pub mod memory_tools;
pub mod net_utils;
pub mod patch;
pub mod path_utils;
pub mod registry;
pub mod terminal;
pub mod vision;
pub mod web_extract;
pub mod web_search;
pub use registry::{ToolRegistration, ToolRegistry};
