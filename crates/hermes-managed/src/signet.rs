use std::{
    collections::HashMap,
    path::Path,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use hermes_config::config::AppConfig;
use hermes_core::{
    error::{HermesError, Result},
    stream::StreamDelta,
    tool::{
        ToolContext, ToolExecutionObservation, ToolExecutionObserver,
        ToolExecutionResultObservation,
    },
};
use signet_core::{
    Action, SignetError, audit, generate_and_save, load_key_info, load_signing_key, sign,
    sign_compound,
};

pub fn build_signet_observer(
    app_config: &AppConfig,
) -> Result<Option<Arc<dyn ToolExecutionObserver>>> {
    if !app_config.signet.enabled {
        return Ok(None);
    }

    let dir = app_config.signet_dir();
    ensure_signet_key(&dir, &app_config.signet.key_name, &app_config.signet.owner)?;

    Ok(Some(Arc::new(ManagedSignetObserver {
        key_name: app_config.signet.key_name.clone(),
        owner: app_config.signet.owner.clone(),
        dir,
        pending: Mutex::new(HashMap::new()),
    })))
}

fn ensure_signet_key(dir: &Path, key_name: &str, owner: &str) -> Result<()> {
    match load_key_info(dir, key_name) {
        Ok(_) => Ok(()),
        Err(SignetError::KeyNotFound(_)) => {
            generate_and_save(dir, key_name, Some(owner), None, None)
                .map(|_| ())
                .map_err(|err| signet_config_error("failed to create Signet key", err))
        }
        Err(err) => Err(signet_config_error("failed to load Signet key", err)),
    }
}

fn signet_config_error(context: &str, err: SignetError) -> HermesError {
    HermesError::Config(format!("{context}: {err}"))
}

struct ManagedSignetObserver {
    key_name: String,
    owner: String,
    dir: PathBuf,
    pending: Mutex<HashMap<String, PendingSignetCall>>,
}

#[derive(Clone)]
struct PendingSignetCall {
    ts_request: String,
    request_receipt_id: String,
    request_record_hash: String,
}

struct SignetReceiptRecord {
    receipt_id: String,
    record_hash: String,
    receipt_version: u8,
    response_hash: Option<String>,
}

impl ManagedSignetObserver {
    fn sign_request_receipt(
        &self,
        observation: &ToolExecutionObservation,
    ) -> std::result::Result<(PendingSignetCall, SignetReceiptRecord), SignetError> {
        let signing_key = load_signing_key(&self.dir, &self.key_name, None)?;
        let receipt = sign(
            &signing_key,
            &action_from_observation(observation),
            &self.key_name,
            &self.owner,
        )?;
        let record = audit::append(&self.dir, &serde_json::to_value(&receipt)?)?;
        let pending = PendingSignetCall {
            ts_request: receipt.ts.clone(),
            request_receipt_id: receipt.id.clone(),
            request_record_hash: record.record_hash.clone(),
        };
        let emitted = SignetReceiptRecord {
            receipt_id: receipt.id,
            record_hash: record.record_hash,
            receipt_version: 1,
            response_hash: None,
        };
        Ok((pending, emitted))
    }

    fn sign_response_receipt(
        &self,
        observation: &ToolExecutionResultObservation,
        pending: &PendingSignetCall,
    ) -> std::result::Result<SignetReceiptRecord, SignetError> {
        let signing_key = load_signing_key(&self.dir, &self.key_name, None)?;
        let ts_response = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let response = tool_result_payload(&observation.result);
        let receipt = sign_compound(
            &signing_key,
            &action_from_observation(&observation.request),
            &response,
            &self.key_name,
            &self.owner,
            &pending.ts_request,
            &ts_response,
        )?;
        let response_hash = receipt.response.content_hash.clone();
        let record = audit::append(&self.dir, &serde_json::to_value(&receipt)?)?;
        Ok(SignetReceiptRecord {
            receipt_id: receipt.id,
            record_hash: record.record_hash,
            receipt_version: 2,
            response_hash: Some(response_hash),
        })
    }

    async fn emit_signet_event(
        &self,
        ctx: &ToolContext,
        kind: &str,
        observation: &ToolExecutionObservation,
        message: Option<String>,
        metadata: serde_json::Value,
    ) {
        let _ = ctx
            .delta_tx
            .send(StreamDelta::ToolEvent {
                kind: kind.to_string(),
                tool: observation.tool_name.clone(),
                call_id: Some(observation.call_id.clone()),
                message,
                metadata: Some(metadata),
            })
            .await;
    }

    async fn emit_signet_error(
        &self,
        ctx: &ToolContext,
        observation: &ToolExecutionObservation,
        err: SignetError,
    ) {
        let _ = ctx
            .delta_tx
            .send(StreamDelta::ToolProgress {
                tool: observation.tool_name.clone(),
                status: format!("signet error: {err}"),
            })
            .await;
    }
}

#[async_trait]
impl ToolExecutionObserver for ManagedSignetObserver {
    async fn on_tool_call(
        &self,
        observation: ToolExecutionObservation,
        ctx: &ToolContext,
    ) -> Result<()> {
        match self.sign_request_receipt(&observation) {
            Ok((pending, record)) => {
                self.pending
                    .lock()
                    .expect("signet pending map poisoned")
                    .insert(observation.call_id.clone(), pending);
                self.emit_signet_event(
                    ctx,
                    "tool.request_signed",
                    &observation,
                    Some("Signet request receipt appended".to_string()),
                    serde_json::json!({
                        "provider": "signet",
                        "receipt_id": record.receipt_id,
                        "receipt_version": record.receipt_version,
                        "record_hash": record.record_hash,
                        "key_name": self.key_name,
                    }),
                )
                .await;
            }
            Err(err) => self.emit_signet_error(ctx, &observation, err).await,
        }

        Ok(())
    }

    async fn on_tool_result(
        &self,
        observation: ToolExecutionResultObservation,
        ctx: &ToolContext,
    ) -> Result<()> {
        let pending = self
            .pending
            .lock()
            .expect("signet pending map poisoned")
            .remove(&observation.request.call_id);

        let Some(pending) = pending else {
            self.emit_signet_error(
                ctx,
                &observation.request,
                SignetError::InvalidReceipt(format!(
                    "missing pending Signet request receipt for call {}",
                    observation.request.call_id
                )),
            )
            .await;
            return Ok(());
        };

        match self.sign_response_receipt(&observation, &pending) {
            Ok(record) => {
                self.emit_signet_event(
                    ctx,
                    "tool.response_signed",
                    &observation.request,
                    Some("Signet response receipt appended".to_string()),
                    serde_json::json!({
                        "provider": "signet",
                        "receipt_id": record.receipt_id,
                        "receipt_version": record.receipt_version,
                        "record_hash": record.record_hash,
                        "response_hash": record.response_hash,
                        "request_receipt_id": pending.request_receipt_id,
                        "request_record_hash": pending.request_record_hash,
                        "key_name": self.key_name,
                        "tool_result_error": observation.result.is_error,
                    }),
                )
                .await;
            }
            Err(err) => self.emit_signet_error(ctx, &observation.request, err).await,
        }

        Ok(())
    }
}

fn action_target(observation: &ToolExecutionObservation) -> String {
    match observation.toolset.as_deref() {
        Some(toolset) if !toolset.is_empty() => {
            format!("hermes://toolset/{toolset}/{}", observation.tool_name)
        }
        _ => format!("hermes://tool/{}", observation.tool_name),
    }
}

fn action_from_observation(observation: &ToolExecutionObservation) -> Action {
    Action {
        tool: observation.tool_name.clone(),
        params: observation.arguments.clone(),
        params_hash: String::new(),
        target: action_target(observation),
        transport: "in_process".to_string(),
        session: Some(observation.session_id.clone()),
        call_id: Some(observation.call_id.clone()),
        response_hash: None,
    }
}

fn tool_result_payload(result: &hermes_core::message::ToolResult) -> serde_json::Value {
    serde_json::from_str(&result.content).unwrap_or_else(|_| {
        serde_json::json!({
            "content": result.content,
            "is_error": result.is_error,
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hermes_core::{
        message::ToolResult,
        stream::StreamDelta,
        tool::{
            ApprovalRequest, ToolConfig, ToolContext, ToolExecutionObservation,
            ToolExecutionResultObservation,
        },
    };
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;

    fn cfg(dir: &TempDir) -> AppConfig {
        AppConfig {
            signet: hermes_config::config::SignetConfig {
                enabled: true,
                key_name: "managed-test".to_string(),
                owner: "qa".to_string(),
                dir: Some(dir.path().to_path_buf()),
            },
            ..AppConfig::default()
        }
    }

    fn make_ctx(delta_tx: mpsc::Sender<StreamDelta>) -> ToolContext {
        let (approval_tx, _) = mpsc::channel::<ApprovalRequest>(1);
        ToolContext {
            session_id: "run_test".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            delta_tx,
            execution_observer: None,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        }
    }

    #[test]
    fn build_signet_observer_creates_key_material() {
        let dir = tempfile::tempdir().unwrap();
        let app_config = cfg(&dir);

        let observer = build_signet_observer(&app_config).unwrap();

        assert!(observer.is_some());
        assert!(dir.path().join("keys/managed-test.key").exists());
        assert!(dir.path().join("keys/managed-test.pub").exists());
    }

    #[tokio::test]
    async fn signet_observer_emits_request_and_response_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let app_config = cfg(&dir);
        let observer = build_signet_observer(&app_config)
            .unwrap()
            .expect("expected signet observer");
        let (delta_tx, mut delta_rx) = mpsc::channel(4);
        let ctx = make_ctx(delta_tx);

        observer
            .on_tool_call(
                ToolExecutionObservation {
                    session_id: "run_test".to_string(),
                    call_id: "call_123".to_string(),
                    tool_name: "read_file".to_string(),
                    toolset: Some("file".to_string()),
                    arguments: serde_json::json!({ "path": "/tmp/demo.txt" }),
                },
                &ctx,
            )
            .await
            .unwrap();

        let request_delta = delta_rx.recv().await.expect("missing request delta");
        match request_delta {
            StreamDelta::ToolEvent {
                kind,
                tool,
                call_id,
                message,
                metadata: Some(metadata),
            } => {
                assert_eq!(kind, "tool.request_signed");
                assert_eq!(tool, "read_file");
                assert_eq!(call_id.as_deref(), Some("call_123"));
                assert_eq!(message.as_deref(), Some("Signet request receipt appended"));
                assert_eq!(metadata["provider"], "signet");
                assert_eq!(metadata["receipt_version"], 1);
                assert!(metadata["receipt_id"].as_str().unwrap().starts_with("rec_"));
                assert!(
                    metadata["record_hash"]
                        .as_str()
                        .unwrap()
                        .starts_with("sha256:")
                );
            }
            other => panic!("unexpected delta: {other:?}"),
        }

        observer
            .on_tool_result(
                ToolExecutionResultObservation {
                    request: ToolExecutionObservation {
                        session_id: "run_test".to_string(),
                        call_id: "call_123".to_string(),
                        tool_name: "read_file".to_string(),
                        toolset: Some("file".to_string()),
                        arguments: serde_json::json!({ "path": "/tmp/demo.txt" }),
                    },
                    result: ToolResult::ok(serde_json::json!({ "content": "ok" }).to_string()),
                },
                &ctx,
            )
            .await
            .unwrap();

        let response_delta = delta_rx.recv().await.expect("missing response delta");
        match response_delta {
            StreamDelta::ToolEvent {
                kind,
                tool,
                call_id,
                message,
                metadata: Some(metadata),
            } => {
                assert_eq!(kind, "tool.response_signed");
                assert_eq!(tool, "read_file");
                assert_eq!(call_id.as_deref(), Some("call_123"));
                assert_eq!(message.as_deref(), Some("Signet response receipt appended"));
                assert_eq!(metadata["provider"], "signet");
                assert_eq!(metadata["receipt_version"], 2);
                assert!(metadata["receipt_id"].as_str().unwrap().starts_with("rec_"));
                assert!(
                    metadata["response_hash"]
                        .as_str()
                        .unwrap()
                        .starts_with("sha256:")
                );
                assert_eq!(metadata["tool_result_error"], false);
                assert!(
                    metadata["request_receipt_id"]
                        .as_str()
                        .unwrap()
                        .starts_with("rec_")
                );
            }
            other => panic!("unexpected delta: {other:?}"),
        }

        let audit_dir = dir.path().join("audit");
        let audit_files: Vec<_> = std::fs::read_dir(&audit_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        assert_eq!(audit_files.len(), 1);
        let audit_file = &audit_files[0];
        let contents = std::fs::read_to_string(audit_file).unwrap();
        assert!(contents.contains("\"call_id\":\"call_123\""));
        assert!(contents.contains("\"tool\":\"read_file\""));
        assert!(contents.contains("\"content_hash\":\"sha256:"));
    }
}
