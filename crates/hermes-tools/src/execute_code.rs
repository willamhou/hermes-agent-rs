use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::json;

use crate::approval_key::approval_memory_key;
use crate::process_handoff::{
    CompletedProcessCapture, emit_process_completed, emit_process_failed, emit_process_started,
    emit_process_timed_out,
};
use crate::process_registry::{configure_child_process_group, kill_process_group};
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
        configure_child_process_group(&mut cmd);

        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_file);
                return Ok(ToolResult::error(format!("failed to spawn python3: {e}")));
            }
        };

        let child_id = child.id();
        let cleanup_registration = child_id.and_then(|pid| {
            session_cleanup::register_process_group(&ctx.session_id, pid, "execute_code")
        });
        emit_process_started(ctx, self.name(), child_id, timeout_secs).await;
        let output =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
            {
                Err(_) => {
                    if let Some(pid) = child_id {
                        let _ = kill_process_group(pid);
                    }
                    emit_process_timed_out(ctx, self.name(), child_id, timeout_secs).await;
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
                    emit_process_failed(
                        ctx,
                        self.name(),
                        child_id,
                        format!("process wait failed: {e}"),
                    )
                    .await;
                    if let Some(registration) = cleanup_registration.as_ref() {
                        let _ = session_cleanup::unregister(registration);
                    }
                    let _ = std::fs::remove_file(&temp_file);
                    return Ok(ToolResult::error(format!("python execution failed: {e}")));
                }
                Ok(Ok(output)) => {
                    emit_process_completed(
                        ctx,
                        self.name(),
                        CompletedProcessCapture {
                            process_group: child_id,
                            exit_code: output.status.code(),
                            stdout_chars: output.stdout.len(),
                            stderr_chars: output.stderr.len(),
                            stdout_preview: Some(truncate_output(
                                &String::from_utf8_lossy(&output.stdout),
                                2_000,
                            )),
                            stderr_preview: Some(truncate_output(
                                &String::from_utf8_lossy(&output.stderr),
                                2_000,
                            )),
                        },
                    )
                    .await;
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

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, LazyLock},
    };

    use super::*;
    use hermes_core::tool::{ToolConfig, ToolContext};
    use tempfile::{NamedTempFile, TempDir};
    use tokio::sync::mpsc;
    use tokio::time::Duration;

    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    fn make_ctx(
        working_dir: PathBuf,
    ) -> (
        ToolContext,
        mpsc::Receiver<hermes_core::tool::ApprovalRequest>,
        mpsc::Receiver<hermes_core::stream::StreamDelta>,
    ) {
        let (approval_tx, approval_rx) = mpsc::channel(8);
        let (delta_tx, delta_rx) = mpsc::channel(8);
        let config = ToolConfig {
            workspace_root: working_dir.clone(),
            ..ToolConfig::default()
        };
        let ctx = ToolContext {
            session_id: "execute-code-test-session".to_string(),
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
        };
        (ctx, approval_rx, delta_rx)
    }

    fn pid_is_alive(pid: u32) -> bool {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }

    async fn wait_for_pid_file(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn execute_code_timeout_kills_process_group_descendants() {
        let _guard = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::set("HERMES_ENABLE_EXECUTE_CODE", "1");
        let tmp = TempDir::new().unwrap();
        let pid_file = NamedTempFile::new().unwrap();
        let (ctx, mut approval_rx, _delta_rx) = make_ctx(tmp.path().to_path_buf());
        let code = format!(
            r#"
import pathlib
import subprocess
import time

pid_file = pathlib.Path(r"{pid_file}")
child = subprocess.Popen(["sleep", "30"])
pid_file.write_text(str(child.pid))
time.sleep(30)
"#,
            pid_file = pid_file.path().display(),
        );

        let handle = tokio::spawn(async move {
            let tool = ExecuteCodeTool;
            tool.execute(json!({"code": code, "timeout": 1}), &ctx)
                .await
        });

        let request = approval_rx.recv().await.expect("approval request");
        let _ = request.response_tx.send(ApprovalDecision::Allow);

        let result = handle.await.unwrap().unwrap();
        assert!(!result.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["exit_code"], 124);
        assert_eq!(parsed["stderr"], "python execution timed out");

        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !pid_is_alive(descendant_pid),
            "descendant pid {descendant_pid} should be terminated with the python process group"
        );
    }

    #[tokio::test]
    async fn execute_code_emits_process_handoff_events() {
        let _guard = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::set("HERMES_ENABLE_EXECUTE_CODE", "1");
        let tmp = TempDir::new().unwrap();
        let (ctx, mut approval_rx, mut delta_rx) = make_ctx(tmp.path().to_path_buf());

        let handle = tokio::spawn(async move {
            let tool = ExecuteCodeTool;
            tool.execute(json!({"code": "print('ok')", "timeout": 5}), &ctx)
                .await
        });

        let request = approval_rx.recv().await.expect("approval request");
        let _ = request.response_tx.send(ApprovalDecision::Allow);

        let started = delta_rx.recv().await.expect("process started event");
        match started {
            hermes_core::stream::StreamDelta::ToolEvent {
                kind,
                tool,
                metadata: Some(metadata),
                ..
            } => {
                assert_eq!(kind, "tool.process_started");
                assert_eq!(tool, "execute_code");
                assert_eq!(metadata["state"], "started");
            }
            other => panic!("unexpected delta: {other:?}"),
        }

        let completed = delta_rx.recv().await.expect("process completed event");
        match completed {
            hermes_core::stream::StreamDelta::ToolEvent {
                kind,
                tool,
                metadata: Some(metadata),
                ..
            } => {
                assert_eq!(kind, "tool.process_completed");
                assert_eq!(tool, "execute_code");
                assert_eq!(metadata["state"], "completed");
                assert_eq!(metadata["exit_code"], 0);
                assert_eq!(metadata["stdout_preview"], "ok\n");
            }
            other => panic!("unexpected delta: {other:?}"),
        }

        let result = handle.await.unwrap().unwrap();
        assert!(!result.is_error);
    }
}
