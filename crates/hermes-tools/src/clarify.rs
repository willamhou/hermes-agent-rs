use async_trait::async_trait;

use hermes_core::{
    clarify::{ClarifyRequest, ClarifyResponse},
    error::{HermesError, Result},
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

pub struct ClarifyTool;

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str {
        "clarify"
    }

    fn toolset(&self) -> &str {
        "interaction"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "clarify".into(),
            description: "Ask the user a clarifying question. Use when you need more information before proceeding.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user"
                    },
                    "choices": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Up to 4 predefined answer choices. Omit for open-ended questions."
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        // 1. Check delegation depth — clarify blocked in child agents
        if ctx.delegation_depth > 0 {
            return Ok(ToolResult::error(
                "clarify is not available in delegated tasks",
            ));
        }

        // 2. Check clarify channel exists
        let clarify_tx = match &ctx.clarify_tx {
            Some(tx) => tx,
            None => return Ok(ToolResult::error("no clarify handler configured")),
        };

        // 3. Parse args
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HermesError::Tool {
                name: "clarify".into(),
                message: "question is required".into(),
            })?
            .to_string();

        let choices: Vec<String> = args
            .get("choices")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Limit to 4 choices
        let choices: Vec<String> = choices.into_iter().take(4).collect();

        // 4. Send request and await response
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let request = ClarifyRequest {
            question: question.clone(),
            choices: choices.clone(),
            response_tx,
        };

        clarify_tx
            .send(request)
            .await
            .map_err(|_| HermesError::Tool {
                name: "clarify".into(),
                message: "clarify handler disconnected".into(),
            })?;

        // 5. Wait for response (with timeout)
        let response = tokio::time::timeout(std::time::Duration::from_secs(120), response_rx).await;

        match response {
            Ok(Ok(ClarifyResponse::Answer(answer))) => Ok(ToolResult::ok(
                serde_json::json!({
                    "question": question,
                    "choices_offered": choices,
                    "user_response": answer,
                })
                .to_string(),
            )),
            Ok(Ok(ClarifyResponse::Timeout)) | Err(_) => Ok(ToolResult::ok(
                serde_json::json!({
                    "question": question,
                    "choices_offered": choices,
                    "user_response": null,
                    "timed_out": true,
                })
                .to_string(),
            )),
            Ok(Err(_)) => Ok(ToolResult::error(
                "clarify handler dropped response channel",
            )),
        }
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(ClarifyTool) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use hermes_core::{
        clarify::{ClarifyRequest, ClarifyResponse},
        tool::ToolConfig,
    };
    use tokio::sync::mpsc;

    fn make_ctx_with_clarify(depth: u32) -> (ToolContext, mpsc::Receiver<ClarifyRequest>) {
        let (clarify_tx, clarify_rx) = mpsc::channel(4);
        let (approval_tx, _) = mpsc::channel(1);
        let (delta_tx, _) = mpsc::channel(1);
        let ctx = ToolContext {
            session_id: "test".into(),
            working_dir: std::path::PathBuf::from("/tmp"),
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: depth,
            clarify_tx: Some(clarify_tx),
        };
        (ctx, clarify_rx)
    }

    #[tokio::test]
    async fn test_clarify_blocked_in_delegation() {
        let (ctx, _rx) = make_ctx_with_clarify(1); // depth=1
        let tool = ClarifyTool;
        let result = tool
            .execute(serde_json::json!({"question": "test?"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not available in delegated"));
    }

    #[tokio::test]
    async fn test_clarify_no_handler() {
        // ctx with clarify_tx = None
        let (approval_tx, _) = mpsc::channel(1);
        let (delta_tx, _) = mpsc::channel(1);
        let ctx = ToolContext {
            session_id: "test".into(),
            working_dir: std::path::PathBuf::from("/tmp"),
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        };
        let tool = ClarifyTool;
        let result = tool
            .execute(serde_json::json!({"question": "test?"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("no clarify handler"));
    }

    #[tokio::test]
    async fn test_clarify_answer() {
        let (ctx, mut rx) = make_ctx_with_clarify(0);
        let tool = ClarifyTool;

        // Spawn responder
        tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                assert_eq!(req.question, "Pick a color?");
                assert_eq!(req.choices, vec!["red", "blue"]);
                let _ = req.response_tx.send(ClarifyResponse::Answer("blue".into()));
            }
        });

        let result = tool
            .execute(
                serde_json::json!({
                    "question": "Pick a color?",
                    "choices": ["red", "blue"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["user_response"], "blue");
        assert_eq!(parsed["question"], "Pick a color?");
    }

    #[tokio::test]
    async fn test_clarify_timeout() {
        let (ctx, mut rx) = make_ctx_with_clarify(0);
        let tool = ClarifyTool;

        // Spawn responder that sends Timeout
        tokio::spawn(async move {
            if let Some(req) = rx.recv().await {
                let _ = req.response_tx.send(ClarifyResponse::Timeout);
            }
        });

        let result = tool
            .execute(serde_json::json!({"question": "test?"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error); // timeout is not an error, just a note
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed["timed_out"], true);
    }
}
