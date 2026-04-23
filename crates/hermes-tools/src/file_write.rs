use async_trait::async_trait;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::path_utils;

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn toolset(&self) -> &str {
        "file"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".to_string(),
            description:
                "Write content to a file, creating it and any parent directories if needed."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to write"
                    },
                    "content": {
                        "type": "string",
                        "description": "Content to write to the file"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        // Parse args
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => return Ok(ToolResult::error("missing required parameter: path")),
        };

        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return Ok(ToolResult::error("missing required parameter: content")),
        };

        // Resolve path
        let resolved = path_utils::resolve_path(&path_str, &ctx.working_dir);

        // Safety checks
        if let Err(e) = crate::path_utils::check_sandbox(&resolved, &ctx.tool_config.workspace_root)
        {
            return Ok(ToolResult::error(e));
        }

        if path_utils::is_blocked_write_path(&resolved, &ctx.tool_config) {
            return Ok(ToolResult::error("write to this path is blocked"));
        }

        // Check if file already exists
        let created = !resolved.exists();

        // Create parent directories
        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(ToolResult::error(format!(
                    "failed to create parent directories: {e}"
                )));
            }
        }

        // Write file
        let bytes_written = content.len();
        if let Err(e) = std::fs::write(&resolved, &content) {
            return Ok(ToolResult::error(format!("failed to write file: {e}")));
        }

        let result = json!({
            "path": resolved.to_string_lossy(),
            "bytes_written": bytes_written,
            "created": created
        });

        Ok(ToolResult::ok(result.to_string()))
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(WriteFileTool) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool::ToolConfig;
    use std::sync::Arc;

    fn make_ctx(working_dir: std::path::PathBuf) -> ToolContext {
        make_ctx_with_root(working_dir.clone(), working_dir)
    }

    fn make_ctx_with_root(
        working_dir: std::path::PathBuf,
        workspace_root: std::path::PathBuf,
    ) -> ToolContext {
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, _delta_rx) = tokio::sync::mpsc::channel(8);
        let config = ToolConfig {
            workspace_root,
            ..ToolConfig::default()
        };
        ToolContext {
            session_id: "test-session".to_string(),
            working_dir,
            approval_tx,
            delta_tx,
            execution_observer: None,
            tool_config: Arc::new(config),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        }
    }

    #[tokio::test]
    async fn test_write_file_create() {
        let tmp = std::env::temp_dir().join(format!("hermes_write_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("hello.txt");
        let ctx = make_ctx(tmp.clone());
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "hello world"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "unexpected error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["created"], true);
        assert_eq!(parsed["bytes_written"], 11);

        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(on_disk, "hello world");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_write_file_overwrite() {
        let tmp = std::env::temp_dir().join(format!("hermes_write_over_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("overwrite.txt");
        std::fs::write(&file_path, "original content").unwrap();

        let ctx = make_ctx(tmp.clone());
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "new content"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "unexpected error: {}", result.content);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["created"], false);

        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(on_disk, "new content");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let tmp = std::env::temp_dir().join(format!("hermes_write_dirs_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        let file_path = tmp.join("nested/path/file.txt");
        let ctx = make_ctx(tmp.clone());
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "content": "deep file"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error, "unexpected error: {}", result.content);

        let on_disk = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(on_disk, "deep file");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn test_write_file_blocked_etc() {
        let ctx = make_ctx(std::path::PathBuf::from("/tmp"));
        let tool = WriteFileTool;
        let args = serde_json::json!({
            "path": "/etc/test",
            "content": "should not write"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(result.is_error);
        // sandbox check fires before is_blocked_write_path
        assert!(
            result.content.contains("blocked") || result.content.contains("escapes workspace root")
        );
    }

    #[tokio::test]
    async fn test_write_file_sandbox_escape() {
        let tmp = std::env::temp_dir().join(format!("hermes_sandbox_write_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        // Workspace root is tmp; try to write outside it
        let ctx = make_ctx(tmp.clone());
        let tool = WriteFileTool;
        let outside = std::env::temp_dir().join("hermes_escape_target.txt");
        let args = serde_json::json!({
            "path": outside.to_str().unwrap(),
            "content": "should not write"
        });
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(
            result.is_error,
            "expected sandbox error, got: {}",
            result.content
        );
        assert!(
            result.content.contains("escapes workspace root"),
            "unexpected error: {}",
            result.content
        );
        // Ensure the file was not created
        assert!(!outside.exists(), "file should not have been created");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
