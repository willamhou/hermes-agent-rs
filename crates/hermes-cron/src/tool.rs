use std::path::PathBuf;

use async_trait::async_trait;
use hermes_core::error::{HermesError, Result};
use hermes_core::message::ToolResult;
use hermes_core::tool::{Tool, ToolContext, ToolSchema};
use serde_json::json;

use crate::job::{CronJob, compute_next_run, parse_schedule};
use crate::store::JobStore;

pub struct CronTool {
    store_path: PathBuf,
}

impl CronTool {
    pub fn new(store_path: PathBuf) -> Self {
        Self { store_path }
    }

    fn open_store(&self) -> Result<JobStore> {
        JobStore::open(self.store_path.clone()).map_err(|e| HermesError::Tool {
            name: "cron".into(),
            message: format!("store error: {e}"),
        })
    }
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn toolset(&self) -> &str {
        "scheduling"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "cron".into(),
            description:
                "Manage scheduled tasks. Create, list, remove, pause, resume, or trigger jobs."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "list", "remove", "pause", "resume", "trigger"],
                        "description": "Action to perform"
                    },
                    "prompt": { "type": "string", "description": "Task prompt (for create)" },
                    "schedule": { "type": "string", "description": "Schedule: '30m', '2h', '0 9 * * *', or ISO timestamp" },
                    "name": { "type": "string", "description": "Job name (for create)" },
                    "deliver": { "type": "string", "description": "Delivery target (default: 'local')" },
                    "job_id": { "type": "string", "description": "Job ID (for remove/pause/resume/trigger)" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let store = self.open_store()?;

        match action {
            "create" => {
                let prompt = args.get("prompt").and_then(|v| v.as_str()).ok_or_else(|| {
                    HermesError::Tool {
                        name: "cron".into(),
                        message: "prompt required for create".into(),
                    }
                })?;
                let schedule_str =
                    args.get("schedule")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| HermesError::Tool {
                            name: "cron".into(),
                            message: "schedule required for create".into(),
                        })?;
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unnamed job");
                let deliver = args
                    .get("deliver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("local");

                let schedule = parse_schedule(schedule_str).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: format!("invalid schedule: {e}"),
                })?;

                let job = CronJob::new(name.into(), prompt.into(), schedule, deliver.into());
                let job_id = job.id.clone();
                let next = job.next_run_at.clone();
                store.create(job).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;

                Ok(ToolResult::ok(
                    json!({
                        "created": true,
                        "job_id": job_id,
                        "next_run_at": next,
                    })
                    .to_string(),
                ))
            }
            "list" => {
                let jobs = store.list().map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;
                let entries: Vec<serde_json::Value> = jobs
                    .iter()
                    .map(|j| {
                        json!({
                            "id": j.id,
                            "name": j.name,
                            "enabled": j.enabled,
                            "schedule": j.schedule,
                            "next_run_at": j.next_run_at,
                            "last_status": j.last_status,
                        })
                    })
                    .collect();
                let count = entries.len();
                Ok(ToolResult::ok(
                    json!({"jobs": entries, "count": count}).to_string(),
                ))
            }
            "remove" => {
                let job_id = args.get("job_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    HermesError::Tool {
                        name: "cron".into(),
                        message: "job_id required".into(),
                    }
                })?;
                let removed = store.remove(job_id).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;
                Ok(ToolResult::ok(
                    json!({"removed": removed, "job_id": job_id}).to_string(),
                ))
            }
            "pause" => {
                let job_id = args.get("job_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    HermesError::Tool {
                        name: "cron".into(),
                        message: "job_id required".into(),
                    }
                })?;
                let mut job = store
                    .get(job_id)
                    .map_err(|e| HermesError::Tool {
                        name: "cron".into(),
                        message: e.to_string(),
                    })?
                    .ok_or_else(|| HermesError::Tool {
                        name: "cron".into(),
                        message: format!("job {job_id} not found"),
                    })?;
                job.enabled = false;
                let found = store.update(job).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;
                if !found {
                    tracing::warn!(job_id = %job_id, "pause: job disappeared from store");
                }
                Ok(ToolResult::ok(
                    json!({"paused": true, "job_id": job_id}).to_string(),
                ))
            }
            "resume" => {
                let job_id = args.get("job_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    HermesError::Tool {
                        name: "cron".into(),
                        message: "job_id required".into(),
                    }
                })?;
                let mut job = store
                    .get(job_id)
                    .map_err(|e| HermesError::Tool {
                        name: "cron".into(),
                        message: e.to_string(),
                    })?
                    .ok_or_else(|| HermesError::Tool {
                        name: "cron".into(),
                        message: format!("job {job_id} not found"),
                    })?;
                job.enabled = true;
                // Recompute next_run
                let now = chrono::Utc::now();
                job.next_run_at = compute_next_run(&job.schedule, &now).map(|dt| dt.to_rfc3339());
                let found = store.update(job).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;
                if !found {
                    tracing::warn!(job_id = %job_id, "resume: job disappeared from store");
                }
                Ok(ToolResult::ok(
                    json!({"resumed": true, "job_id": job_id}).to_string(),
                ))
            }
            "trigger" => {
                let job_id = args.get("job_id").and_then(|v| v.as_str()).ok_or_else(|| {
                    HermesError::Tool {
                        name: "cron".into(),
                        message: "job_id required".into(),
                    }
                })?;
                let mut job = store
                    .get(job_id)
                    .map_err(|e| HermesError::Tool {
                        name: "cron".into(),
                        message: e.to_string(),
                    })?
                    .ok_or_else(|| HermesError::Tool {
                        name: "cron".into(),
                        message: format!("job {job_id} not found"),
                    })?;
                job.next_run_at = Some(chrono::Utc::now().to_rfc3339());
                job.enabled = true;
                let found = store.update(job).map_err(|e| HermesError::Tool {
                    name: "cron".into(),
                    message: e.to_string(),
                })?;
                if !found {
                    tracing::warn!(job_id = %job_id, "trigger: job disappeared from store");
                }
                Ok(ToolResult::ok(
                    json!({"triggered": true, "job_id": job_id}).to_string(),
                ))
            }
            _ => Ok(ToolResult::error(format!("unknown action: {action}"))),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    fn make_tool(dir: &TempDir) -> CronTool {
        CronTool::new(dir.path().join("jobs.json"))
    }

    fn make_ctx(dir: &TempDir) -> ToolContext {
        use hermes_core::tool::{ToolConfig, ToolContext};
        let (approval_tx, _) = mpsc::channel(1);
        let (delta_tx, _) = mpsc::channel(1);
        ToolContext {
            session_id: "test".into(),
            working_dir: dir.path().to_path_buf(),
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

    #[tokio::test]
    async fn test_cron_create() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let ctx = make_ctx(&dir);

        let result = tool
            .execute(
                json!({
                    "action": "create",
                    "prompt": "run daily report",
                    "schedule": "1h",
                    "name": "Daily Report"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let val: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(val["created"], true);
        assert!(val["job_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn test_cron_list() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let ctx = make_ctx(&dir);

        for i in 0..2 {
            tool.execute(
                json!({
                    "action": "create",
                    "prompt": format!("task {i}"),
                    "schedule": "30m",
                    "name": format!("Job {i}")
                }),
                &ctx,
            )
            .await
            .unwrap();
        }

        let result = tool.execute(json!({"action": "list"}), &ctx).await.unwrap();

        assert!(!result.is_error);
        let val: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(val["count"], 2);
    }

    #[tokio::test]
    async fn test_cron_remove() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let ctx = make_ctx(&dir);

        let create_result = tool
            .execute(
                json!({
                    "action": "create",
                    "prompt": "removable task",
                    "schedule": "2h",
                    "name": "To Remove"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let val: serde_json::Value = serde_json::from_str(&create_result.content).unwrap();
        let job_id = val["job_id"].as_str().unwrap().to_string();

        let remove_result = tool
            .execute(json!({"action": "remove", "job_id": job_id}), &ctx)
            .await
            .unwrap();

        assert!(!remove_result.is_error);
        let rv: serde_json::Value = serde_json::from_str(&remove_result.content).unwrap();
        assert_eq!(rv["removed"], true);
    }

    #[tokio::test]
    async fn test_cron_pause_resume() {
        let dir = TempDir::new().unwrap();
        let tool = make_tool(&dir);
        let ctx = make_ctx(&dir);

        let create_result = tool
            .execute(
                json!({
                    "action": "create",
                    "prompt": "pausable task",
                    "schedule": "1d",
                    "name": "Pausable"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let val: serde_json::Value = serde_json::from_str(&create_result.content).unwrap();
        let job_id = val["job_id"].as_str().unwrap().to_string();

        // pause
        let pause_result = tool
            .execute(json!({"action": "pause", "job_id": job_id}), &ctx)
            .await
            .unwrap();
        assert!(!pause_result.is_error);
        let pv: serde_json::Value = serde_json::from_str(&pause_result.content).unwrap();
        assert_eq!(pv["paused"], true);

        // verify disabled via list
        let store = JobStore::open(dir.path().join("jobs.json")).unwrap();
        let job = store.get(&job_id).unwrap().unwrap();
        assert!(!job.enabled);

        // resume
        let resume_result = tool
            .execute(json!({"action": "resume", "job_id": job_id}), &ctx)
            .await
            .unwrap();
        assert!(!resume_result.is_error);
        let rv: serde_json::Value = serde_json::from_str(&resume_result.content).unwrap();
        assert_eq!(rv["resumed"], true);

        // verify re-enabled
        let job = store.get(&job_id).unwrap().unwrap();
        assert!(job.enabled);
    }
}
