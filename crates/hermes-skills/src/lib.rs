//! Skill discovery, parsing, injection

pub mod manager;
pub mod skill;
pub mod tools;

pub use manager::{SharedSkillManager, SkillManager};
pub use skill::Skill;
