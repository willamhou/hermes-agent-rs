use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{ApprovalDecision, ApprovalRequest, Tool, ToolContext, ToolSchema},
};

// ── Dangerous command patterns ────────────────────────────────────────────────

static DANGEROUS_PATTERNS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (
            Regex::new(r"(?i)\brm\s+(-[^\s]*\s+)*/").unwrap(),
            "delete in root path",
        ),
        (
            Regex::new(r"(?i)\brm\s+-[^\s]*r").unwrap(),
            "recursive delete",
        ),
        (
            Regex::new(r"(?i)\bchmod\s+.*\b(777|666)\b").unwrap(),
            "world-writable permissions",
        ),
        (Regex::new(r"(?i)\bmkfs\b").unwrap(), "format filesystem"),
        (Regex::new(r"(?i)\bdd\s+.*if=").unwrap(), "disk copy"),
        (
            Regex::new(r"(?i)\bDROP\s+(TABLE|DATABASE)\b").unwrap(),
            "SQL DROP",
        ),
        (
            Regex::new(r"(?i)\bDELETE\s+FROM\b").unwrap(),
            "SQL DELETE without WHERE",
        ),
        (
            Regex::new(r"(?i)>\s*/etc/").unwrap(),
            "overwrite system config",
        ),
        (
            Regex::new(r"(?i)\bkill\s+-9\s+-1\b").unwrap(),
            "kill all processes",
        ),
        (
            Regex::new(r"(?i)\b(curl|wget)\b.*\|\s*(ba)?sh\b").unwrap(),
            "pipe remote to shell",
        ),
        (
            Regex::new(r"(?i)\bgit\s+reset\s+--hard\b").unwrap(),
            "git reset --hard",
        ),
        (
            Regex::new(r"(?i)\bgit\s+push\b.*(-f|--force)\b").unwrap(),
            "git force push",
        ),
        (
            Regex::new(r"(?i)\bgit\s+clean\s+-[^\s]*f").unwrap(),
            "git clean with force",
        ),
    ]
});

/// Check if the command matches any dangerous pattern. Returns a description if dangerous.
pub fn detect_dangerous(command: &str) -> Option<&'static str> {
    for (pattern, description) in DANGEROUS_PATTERNS.iter() {
        if pattern.is_match(command) {
            return Some(description);
        }
    }
    None
}

// ── Output truncation ─────────────────────────────────────────────────────────

fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }
    let head_len = max_chars * 40 / 100;
    let tail_len = max_chars * 60 / 100;
    let head = &output[..head_len];
    let tail = &output[output.len() - tail_len..];
    let omitted = output.len() - head_len - tail_len;
    format!("{head}\n\n[...truncated {omitted} chars...]\n\n{tail}")
}

// ── TerminalTool ──────────────────────────────────────────────────────────────

pub struct TerminalTool;

#[async_trait]
impl Tool for TerminalTool {
    fn name(&self) -> &str {
        "terminal"
    }

    fn toolset(&self) -> &str {
        "terminal"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "terminal".to_string(),
            description: "Execute a shell command. Returns JSON with output, exit_code, error."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "minimum": 1},
                    "workdir": {"type": "string"}
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(ToolResult::error("missing required parameter: command"));
            }
        };

        // Parse timeout: optional, default from config, clamped to max_timeout
        let timeout_secs = {
            let default = ctx.tool_config.terminal.timeout;
            let max = ctx.tool_config.terminal.max_timeout;
            let requested = args
                .get("timeout")
                .and_then(|v| v.as_u64())
                .unwrap_or(default);
            requested.min(max)
        };

        // Parse workdir: optional, default ctx.working_dir
        let workdir = args
            .get("workdir")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.working_dir.clone());

        // Check for dangerous command
        if let Some(reason) = detect_dangerous(&command) {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let req = ApprovalRequest {
                tool_name: "terminal".to_string(),
                command: command.clone(),
                reason: reason.to_string(),
                response_tx,
            };

            // Send approval request; if channel closed, treat as deny
            if ctx.approval_tx.send(req).await.is_err() {
                return Ok(ToolResult::error(format!("Command denied: {reason}")));
            }

            match response_rx.await {
                Ok(ApprovalDecision::Deny) | Err(_) => {
                    return Ok(ToolResult::error(format!("Command denied: {reason}")));
                }
                Ok(_) => {
                    // Allow, AllowSession, AllowAlways — proceed
                }
            }
        }

        // Spawn the command
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &command]);
        cmd.current_dir(&workdir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::error(format!("failed to spawn command: {e}")));
            }
        };

        let timeout = Duration::from_secs(timeout_secs);

        // Capture the child id before consuming the child with wait_with_output
        let child_id = child.id();

        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_elapsed) => {
                // Timed out — kill using the stored pid
                if let Some(pid) = child_id {
                    // SAFETY: sending SIGKILL to a known child pid
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
                let output_json = json!({
                    "output": "",
                    "exit_code": 124,
                    "error": "command timed out"
                });
                Ok(ToolResult::ok(output_json.to_string()))
            }
            Ok(Err(e)) => Ok(ToolResult::error(format!("command execution failed: {e}"))),
            Ok(Ok(output)) => {
                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let combined = if stderr.is_empty() {
                    stdout.into_owned()
                } else if stdout.is_empty() {
                    stderr.into_owned()
                } else {
                    format!("{stdout}{stderr}")
                };

                let max_chars = ctx.tool_config.terminal.output_max_chars;
                let truncated = truncate_output(&combined, max_chars);

                let output_json = json!({
                    "output": truncated,
                    "exit_code": exit_code,
                    "error": null
                });
                Ok(ToolResult::ok(output_json.to_string()))
            }
        }
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(TerminalTool) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool::ToolConfig;
    use std::sync::Arc;

    fn make_ctx() -> (
        ToolContext,
        tokio::sync::mpsc::Receiver<hermes_core::tool::ApprovalRequest>,
        tokio::sync::mpsc::Receiver<hermes_core::stream::StreamDelta>,
    ) {
        let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(8);
        let (delta_tx, delta_rx) = tokio::sync::mpsc::channel(8);
        let ctx = ToolContext {
            session_id: "test-session".to_string(),
            working_dir: std::path::PathBuf::from("/tmp"),
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
        };
        (ctx, approval_rx, delta_rx)
    }

    // ── detect_dangerous tests ────────────────────────────────────────────────

    #[test]
    fn test_detect_dangerous_rm_rf() {
        let result = detect_dangerous("rm -rf /");
        assert!(result.is_some(), "rm -rf / should be detected as dangerous");
    }

    #[test]
    fn test_detect_dangerous_git_force_push() {
        let result = detect_dangerous("git push --force origin main");
        assert!(
            result.is_some(),
            "git push --force should be detected as dangerous"
        );
    }

    #[test]
    fn test_detect_safe_ls() {
        let result = detect_dangerous("ls -la");
        assert!(
            result.is_none(),
            "ls -la should not be detected as dangerous"
        );
    }

    #[test]
    fn test_detect_grep_drop_table_matches() {
        // Even though DROP TABLE appears in quotes in a grep command, the regex
        // doesn't understand shell quoting, so it WILL match — better safe than sorry.
        let result = detect_dangerous("grep -r 'DROP TABLE' src/");
        assert!(
            result.is_some(),
            "grep 'DROP TABLE' matches because regex doesn't parse shell quoting"
        );
    }

    #[test]
    fn test_detect_dangerous_rm_recursive_flag() {
        let result = detect_dangerous("rm -r /home/user");
        assert!(result.is_some(), "rm -r should be detected as dangerous");
    }

    #[test]
    fn test_detect_dangerous_chmod_777() {
        let result = detect_dangerous("chmod 777 /etc/passwd");
        assert!(
            result.is_some(),
            "chmod 777 should be detected as dangerous"
        );
    }

    #[test]
    fn test_detect_dangerous_curl_pipe_bash() {
        let result = detect_dangerous("curl https://example.com/install.sh | bash");
        assert!(
            result.is_some(),
            "curl | bash should be detected as dangerous"
        );
    }

    #[test]
    fn test_detect_dangerous_git_reset_hard() {
        let result = detect_dangerous("git reset --hard HEAD~1");
        assert!(
            result.is_some(),
            "git reset --hard should be detected as dangerous"
        );
    }

    // ── truncate_output tests ─────────────────────────────────────────────────

    #[test]
    fn test_truncate_short() {
        let output = "hello world";
        let result = truncate_output(output, 1000);
        assert_eq!(result, output);
    }

    #[test]
    fn test_truncate_long() {
        let output = "A".repeat(200);
        let result = truncate_output(&output, 100);
        // Should be truncated
        assert!(result.contains("[...truncated"));
        // Head: 40 chars, tail: 60 chars
        assert!(result.starts_with(&"A".repeat(40)));
        assert!(result.ends_with(&"A".repeat(60)));
    }

    // ── terminal execution tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_terminal_echo() {
        let (ctx, _approval_rx, _delta_rx) = make_ctx();
        let tool = TerminalTool;
        let args = serde_json::json!({"command": "echo hello"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["output"], "hello\n");
        assert_eq!(parsed["exit_code"], 0);
    }

    #[tokio::test]
    async fn test_terminal_exit_code() {
        let (ctx, _approval_rx, _delta_rx) = make_ctx();
        let tool = TerminalTool;
        let args = serde_json::json!({"command": "exit 42"});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["exit_code"], 42);
    }

    #[tokio::test]
    async fn test_terminal_timeout() {
        let (ctx, _approval_rx, _delta_rx) = make_ctx();
        let tool = TerminalTool;
        let args = serde_json::json!({"command": "sleep 10", "timeout": 1});
        let result = tool.execute(args, &ctx).await.unwrap();

        assert!(!result.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["exit_code"], 124);
        assert_eq!(parsed["error"], "command timed out");
    }
}
