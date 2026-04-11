//! Terminal rendering for `StreamDelta` events.

use std::io::Write as _;

use crossterm::{
    execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
};
use hermes_core::stream::StreamDelta;
use tokio::sync::mpsc;

/// Consume a stream of [`StreamDelta`] events and render them to stdout.
///
/// - [`StreamDelta::TextDelta`]       → plain text
/// - [`StreamDelta::ReasoningDelta`]  → dark-grey text
/// - [`StreamDelta::ToolCallStart`]   → `[tool: name]` in yellow
/// - [`StreamDelta::ToolCallArgsDelta`] → skipped (too noisy)
/// - [`StreamDelta::ToolProgress`]    → `[tool: status]` in cyan
/// - [`StreamDelta::Done`]            → newline, then stop
pub async fn render_stream(mut rx: mpsc::Receiver<StreamDelta>) {
    let stdout = std::io::stdout();

    while let Some(delta) = rx.recv().await {
        match delta {
            StreamDelta::TextDelta(text) => {
                let mut lock = stdout.lock();
                let _ = write!(lock, "{text}");
                let _ = lock.flush();
            }
            StreamDelta::ReasoningDelta(text) => {
                let mut lock = stdout.lock();
                let _ = execute!(
                    lock,
                    SetForegroundColor(Color::DarkGrey),
                    Print(&text),
                    ResetColor
                );
                let _ = lock.flush();
            }
            StreamDelta::ToolCallStart { name, .. } => {
                let mut lock = stdout.lock();
                let _ = execute!(
                    lock,
                    SetForegroundColor(Color::Yellow),
                    Print(format!("\n[tool: {name}]")),
                    ResetColor
                );
                let _ = lock.flush();
            }
            StreamDelta::ToolCallArgsDelta { .. } => {
                // Too noisy — skip.
            }
            StreamDelta::ToolProgress { tool, status } => {
                let mut lock = stdout.lock();
                let _ = execute!(
                    lock,
                    SetForegroundColor(Color::Cyan),
                    Print(format!("\n[{tool}: {status}]")),
                    ResetColor
                );
                let _ = lock.flush();
            }
            StreamDelta::Done => {
                // Done marks end of one LLM call, but the agent loop may
                // issue more calls (after tool execution). Don't break here;
                // the renderer stops when the channel sender is dropped.
                let mut lock = stdout.lock();
                let _ = writeln!(lock);
                let _ = lock.flush();
            }
        }
    }
}
