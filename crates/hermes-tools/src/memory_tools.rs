use async_trait::async_trait;
use serde_json::json;

use hermes_core::{
    error::{HermesError, Result},
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

const ENTRY_SEPARATOR: &str = "\n§\n";

pub struct MemoryReadTool;
pub struct MemoryWriteTool;

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory_read"
    }

    fn toolset(&self) -> &str {
        "memory"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Read the current live memory or user profile text.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["memory", "user"]}
                },
                "required": ["target"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(memory) = &ctx.memory else {
            return Ok(ToolResult::error("memory tools unavailable"));
        };
        let Some(target) = args.get("target").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: target"));
        };
        let key = target_to_key(target)?;
        let content = memory.read_live(key)?.unwrap_or_default();
        Ok(ToolResult::ok(
            json!({
                "target": target,
                "content": content
            })
            .to_string(),
        ))
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn toolset(&self) -> &str {
        "memory"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Add, replace, or remove entries from live memory.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                    "target": {"type": "string", "enum": ["memory", "user"]},
                    "content": {"type": "string"},
                    "old_text": {"type": "string"}
                },
                "required": ["action", "target"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(memory) = &ctx.memory else {
            return Ok(ToolResult::error("memory tools unavailable"));
        };
        let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: action"));
        };
        let Some(target) = args.get("target").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: target"));
        };
        let key = target_to_key(target)?;
        let current = memory.read_live(key)?.unwrap_or_default();

        let updated = match action {
            "add" => {
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: content"));
                };
                append_entry(&current, content)
            }
            "replace" => {
                let Some(old_text) = args.get("old_text").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: old_text"));
                };
                let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: content"));
                };
                replace_entry(&current, old_text, content)?
            }
            "remove" => {
                let Some(old_text) = args.get("old_text").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::error("missing required parameter: old_text"));
                };
                remove_entry(&current, old_text)?
            }
            other => return Ok(ToolResult::error(format!("unsupported action: {other}"))),
        };

        memory.write_live(key, &updated)?;
        memory.refresh_snapshot()?;
        memory.on_memory_write(action, target, &updated).await?;

        Ok(ToolResult::ok(
            json!({
                "ok": true,
                "action": action,
                "target": target
            })
            .to_string(),
        ))
    }
}

fn target_to_key(target: &str) -> Result<&'static str> {
    match target {
        "memory" => Ok("MEMORY"),
        "user" => Ok("USER"),
        other => Err(HermesError::Tool {
            name: "memory".to_string(),
            message: format!("unsupported target: {other}"),
        }),
    }
}

fn append_entry(current: &str, content: &str) -> String {
    if current.trim().is_empty() {
        content.to_string()
    } else {
        format!("{current}{ENTRY_SEPARATOR}{content}")
    }
}

fn replace_entry(current: &str, old_text: &str, content: &str) -> Result<String> {
    if !current.contains(old_text) {
        return Err(HermesError::Tool {
            name: "memory_write".to_string(),
            message: "old_text not found".to_string(),
        });
    }
    Ok(current.replacen(old_text, content, 1))
}

fn remove_entry(current: &str, old_text: &str) -> Result<String> {
    let mut entries = current
        .split(ENTRY_SEPARATOR)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let Some(index) = entries.iter().position(|entry| entry.contains(old_text)) else {
        return Err(HermesError::Tool {
            name: "memory_write".to_string(),
            message: "old_text not found".to_string(),
        });
    };
    entries.remove(index);
    Ok(entries.join(ENTRY_SEPARATOR))
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(MemoryReadTool) }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(MemoryWriteTool) }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn remove_entry_deletes_first_matching_entry() {
        let updated = remove_entry("one\n§\ntwo\n§\nthree", "two").unwrap();
        assert_eq!(updated, "one\n§\nthree");
    }

    #[test]
    fn append_entry_keeps_separator() {
        let updated = append_entry("one", "two");
        assert_eq!(updated, "one\n§\ntwo");
    }

    #[test]
    fn append_entry_works_on_empty_memory() {
        let current = Arc::new(std::sync::Mutex::new(String::new()));
        *current.lock().unwrap() = append_entry("", "fresh");
        assert_eq!(&*current.lock().unwrap(), "fresh");
    }
}
