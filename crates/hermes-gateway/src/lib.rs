//! Multi-platform gateway and adapters

pub mod api_server;
pub mod discord;
pub mod message_split;
pub mod runner;
pub mod session;
pub mod telegram;

pub use runner::GatewayRunner;
