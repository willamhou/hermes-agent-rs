use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;

use crate::approval_key::approval_memory_key;
use crate::session_cleanup;
use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{ApprovalDecision, ApprovalRequest, Tool, ToolContext, ToolSchema},
};

pub struct ExecuteCodeTool;

#[async_trait]
impl Tool for ExecuteCodeTool {
    fn name(&self) -> &str {
        "execute_code"
    }

    fn toolset(&self) -> &str {
        "code"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        std::env::var("HERMES_ENABLE_EXECUTE_CODE")
            .map(|value| value == "1")
            .unwrap_or(false)
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Execute a short Python snippet in the workspace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string"},
                    "timeout": {"type": "integer", "default": 30, "maximum": 300}
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        if !self.is_available() {
            return Ok(ToolResult::error(
                "execute_code unavailable: HERMES_ENABLE_EXECUTE_CODE is not set to 1",
            ));
        }

        let Some(code) = args.get("code").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: code"));
        };
        let requested_timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
        let timeout_secs = requested_timeout.min(ctx.tool_config.terminal.max_timeout);

        let temp_file = std::env::temp_dir().join(format!(
            "hermes-exec-{}-{}.py",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        if let Err(e) = std::fs::write(&temp_file, code) {
            return Ok(ToolResult::error(format!("failed to write temp file: {e}")));
        }

        let preview = code.lines().take(5).collect::<Vec<_>>().join("\n");
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let approval = ApprovalRequest {
            tool_name: self.name().to_string(),
            memory_key: approval_memory_key(self.name(), code),
            command: format!("python3 {}", temp_file.display()),
            reason: format!("python code execution requested\n{preview}"),
            response_tx,
        };
        if ctx.approval_tx.send(approval).await.is_err() {
            let _ = std::fs::remove_file(&temp_file);
            return Ok(ToolResult::error("python execution denied"));
        }
        match response_rx.await {
            Ok(ApprovalDecision::Deny) | Err(_) => {
                let _ = std::fs::remove_file(&temp_file);
                return Ok(ToolResult::error("python execution denied"));
            }
            Ok(_) => {}
        }

        let mut cmd = tokio::process::Command::new("python3");
        cmd.arg(&temp_file);
        cmd.current_dir(&ctx.working_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_file);
                return Ok(ToolResult::error(format!("failed to spawn python3: {e}")));
            }
        };

        let child_id = child.id();
        let cleanup_registration = child_id
            .and_then(|pid| session_cleanup::register_pid(&ctx.session_id, pid, "execute_code"));
        let output =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
            {
                Err(_) => {
                    if let Some(pid) = child_id {
                        unsafe {
                            libc::kill(pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                    if let Some(registration) = cleanup_registration.as_ref() {
                        let _ = session_cleanup::unregister(registration);
                    }
                    let _ = std::fs::remove_file(&temp_file);
                    return Ok(ToolResult::ok(
                        json!({
                            "stdout": "",
                            "stderr": "python execution timed out",
                            "exit_code": 124
                        })
                        .to_string(),
                    ));
                }
                Ok(Err(e)) => {
                    if let Some(registration) = cleanup_registration.as_ref() {
                        let _ = session_cleanup::unregister(registration);
                    }
                    let _ = std::fs::remove_file(&temp_file);
                    return Ok(ToolResult::error(format!("python execution failed: {e}")));
                }
                Ok(Ok(output)) => {
                    if let Some(registration) = cleanup_registration.as_ref() {
                        let _ = session_cleanup::unregister(registration);
                    }
                    output
                }
            };

        let _ = std::fs::remove_file(&temp_file);

        let stdout = truncate_output(
            &String::from_utf8_lossy(&output.stdout),
            ctx.tool_config.terminal.output_max_chars,
        );
        let stderr = truncate_output(
            &String::from_utf8_lossy(&output.stderr),
            ctx.tool_config.terminal.output_max_chars,
        );

        Ok(ToolResult::ok(
            json!({
                "stdout": stdout,
                "stderr": stderr,
                "exit_code": output.status.code().unwrap_or(-1)
            })
            .to_string(),
        ))
    }
}

fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output.to_string();
    }

    let head_len = max_chars * 40 / 100;
    let tail_len = max_chars.saturating_sub(head_len);
    let head = output.chars().take(head_len).collect::<String>();
    let tail = output
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();

    format!("{head}\n\n[...truncated...]\n\n{tail}")
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(ExecuteCodeTool) }
}
