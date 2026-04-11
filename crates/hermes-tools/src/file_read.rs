use async_trait::async_trait;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::path_utils;

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn toolset(&self) -> &str {
        "file"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".to_string(),
            description:
                "Read a text file with line numbers. Supports pagination via offset and limit."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Line number to start reading from (1-indexed, default 1)",
                        "default": 1,
                        "minimum": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to read (default 500, max 2000)",
                        "default": 500,
                        "maximum": 2000
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        // Parse args
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Ok(ToolResult::error("missing required parameter: path")),
        };

        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
            .min(2000);

        // Resolve path
        let resolved = path_utils::resolve_path(&path_str, &ctx.working_dir);

        // Safety checks
        if path_utils::is_blocked_device(&resolved) {
            return Ok(ToolResult::error("blocked device path"));
        }

        if path_utils::has_binary_extension(&resolved) {
            return Ok(ToolResult::error("binary file, use a different tool"));
        }

        // Read file
        let content = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::error(format!("failed to read file: {e}"))),
        };

        // Get file size from metadata
        let file_size = std::fs::metadata(&resolved)
            .map(|m| m.len())
            .unwrap_or(content.len() as u64);

        // Split into lines
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // Check total content length against config limit (only when reading from beginning)
        let read_max_chars = ctx.tool_config.file.read_max_chars;
        if offset == 1 && limit >= total_lines as u64 && content.len() > read_max_chars {
            return Ok(ToolResult::error(format!(
                "file is too large ({} chars, limit {}). Use offset and limit parameters to read in chunks",
                content.len(),
                read_max_chars
            )));
        }

        // Apply offset (1-indexed) and limit
        let start_idx = (offset as usize).saturating_sub(1);
        let end_idx = (start_idx + limit as usize).min(total_lines);

        let selected_lines = &lines[start_idx..end_idx];

        // Format lines with line numbers
        let mut formatted = String::new();
        for (i, line) in selected_lines.iter().enumerate() {
            let line_num = start_idx + i + 1;
            formatted.push_str(&format!("{line_num}|{line}\n"));
        }

        let showing_start = start_idx + 1;
        let showing_end = end_idx;
        let showing = format!("lines {showing_start}-{showing_end} of {total_lines}");

        let result = json!({
            "content": formatted,
            "path": resolved.to_string_lossy(),
            "file_size": file_size,
            "total_lines": total_lines,
            "showing": showing
        });

        Ok(ToolResult::ok(result.to_string()))
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(ReadFileTool) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool::ToolConfig;
    use std::io::Write;
    use std::sync::Arc;

    fn make_ctx(working_dir: std::path::PathBuf) -> ToolContext {
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, _delta_rx) = tokio::sync::mpsc::channel(8);
        ToolContext {
            session_id: "test-session".to_string(),
            working_dir,
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
        }
    }

    fn write_temp_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[tokio::test]
    async fn test_read_file_basic() {
        let tmp = std::env::temp_dir().join(format!("hermes_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let content = "line one\nline two\nline three\nline four\nline five\n";
        let file_path = write_temp_file(&tmp, "basic.txt", content);

        let ctx = make_ctx(tmp.clone());
        let tool = ReadFileTool;
        let args = serde_json::json!({"path": file_path.to_str().unwrap()});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "unexpected error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let text = parsed["content"].as_str().unwrap();

        assert!(text.contains("1|line one"), "missing line 1");
        assert!(text.contains("2|line two"), "missing line 2");
        assert!(text.contains("5|line five"), "missing line 5");
        assert_eq!(parsed["total_lines"], 5);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_read_file_offset_limit() {
        let tmp = std::env::temp_dir().join(format!("hermes_test_ol_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let lines: String = (1..=10).map(|i| format!("line {i}\n")).collect();
        let file_path = write_temp_file(&tmp, "paged.txt", &lines);

        let ctx = make_ctx(tmp.clone());
        let tool = ReadFileTool;
        let args = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "offset": 3,
            "limit": 2
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "unexpected error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let text = parsed["content"].as_str().unwrap();

        assert!(text.contains("3|line 3"), "expected line 3");
        assert!(text.contains("4|line 4"), "expected line 4");
        assert!(!text.contains("5|line 5"), "should not include line 5");
        assert!(!text.contains("2|line 2"), "should not include line 2");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_read_file_binary_blocked() {
        let tmp = std::env::temp_dir().join(format!("hermes_test_bin_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Create a .png file (won't be read — just name check)
        let file_path = tmp.join("image.png");
        std::fs::write(&file_path, b"\x89PNG\r\n").unwrap();

        let ctx = make_ctx(tmp.clone());
        let tool = ReadFileTool;
        let args = serde_json::json!({"path": file_path.to_str().unwrap()});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("binary file"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_read_file_device_blocked() {
        let ctx = make_ctx(std::path::PathBuf::from("/tmp"));
        let tool = ReadFileTool;
        let args = serde_json::json!({"path": "/dev/zero"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("blocked device path"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let ctx = make_ctx(std::path::PathBuf::from("/tmp"));
        let tool = ReadFileTool;
        let args = serde_json::json!({"path": "/tmp/nonexistent_hermes_file_xyz_12345.txt"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("failed to read file"));
    }
}
