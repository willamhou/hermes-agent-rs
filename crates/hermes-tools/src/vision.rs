use std::path::Path;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    message::{Content, ContentPart, Message, Role},
    provider::ChatRequest,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::net_utils::{build_safe_client, fetch_with_redirects};
use crate::path_utils;

pub struct VisionAnalyzeTool;

#[async_trait]
impl Tool for VisionAnalyzeTool {
    fn name(&self) -> &str {
        "vision_analyze"
    }

    fn toolset(&self) -> &str {
        "vision"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Analyze an image from a local path or remote URL.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "image_path": {"type": "string"},
                    "question": {"type": "string"}
                },
                "required": ["image_path", "question"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(provider) = ctx.aux_provider.as_ref() else {
            return Ok(ToolResult::error("vision tool unavailable"));
        };
        if !provider.model_info().supports_vision {
            return Ok(ToolResult::error(
                "current provider does not support vision",
            ));
        }

        let Some(image_path) = args.get("image_path").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: image_path"));
        };
        let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: question"));
        };

        let (bytes, mime_type) =
            if image_path.starts_with("http://") || image_path.starts_with("https://") {
                match load_remote_image(image_path).await {
                    Ok(data) => data,
                    Err(e) => return Ok(ToolResult::error(e)),
                }
            } else {
                match load_local_image(image_path, ctx) {
                    Ok(data) => data,
                    Err(e) => return Ok(ToolResult::error(e)),
                }
            };

        let encoded = BASE64.encode(bytes);
        let messages = vec![Message {
            role: Role::User,
            content: Content::Parts(vec![
                ContentPart::Image {
                    data: encoded,
                    media_type: mime_type,
                },
                ContentPart::Text {
                    text: question.to_string(),
                },
            ]),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: None,
        }];

        let request = ChatRequest {
            system: "",
            system_segments: None,
            messages: &messages,
            tools: &[],
            max_tokens: 1024,
            temperature: 0.0,
            reasoning: false,
            stop_sequences: vec![],
        };

        let response = match provider.chat(&request, None).await {
            Ok(response) => response,
            Err(e) => return Ok(ToolResult::error(format!("vision request failed: {e}"))),
        };

        Ok(ToolResult::ok(
            json!({
                "analysis": response.content
            })
            .to_string(),
        ))
    }
}

fn load_local_image(
    image_path: &str,
    ctx: &ToolContext,
) -> std::result::Result<(Vec<u8>, String), String> {
    let resolved = path_utils::resolve_path(image_path, &ctx.working_dir);
    path_utils::check_sandbox(&resolved, &ctx.tool_config.workspace_root)?;
    let mime_type = detect_mime_type(&resolved)?;
    let bytes = std::fs::read(&resolved).map_err(|e| format!("failed to read image: {e}"))?;
    Ok((bytes, mime_type))
}

async fn load_remote_image(url: &str) -> std::result::Result<(Vec<u8>, String), String> {
    let client = build_safe_client().map_err(|e| format!("failed to build client: {e}"))?;
    let (final_url, response) = fetch_with_redirects(&client, url, 5).await?;
    if !response.status().is_success() {
        return Err(format!("fetch failed with status {}", response.status()));
    }
    let mime_type = detect_mime_type_from_url(final_url.path())?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read image body: {e}"))?
        .to_vec();
    Ok((bytes, mime_type))
}

fn detect_mime_type(path: &Path) -> std::result::Result<String, String> {
    detect_mime_type_from_url(path.to_string_lossy().as_ref())
}

fn detect_mime_type_from_url(path: &str) -> std::result::Result<String, String> {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        Ok("image/png".to_string())
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Ok("image/jpeg".to_string())
    } else if lower.ends_with(".gif") {
        Ok("image/gif".to_string())
    } else if lower.ends_with(".webp") {
        Ok("image/webp".to_string())
    } else {
        Err("unsupported image type".to_string())
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(VisionAnalyzeTool) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_supported_types() {
        assert_eq!(detect_mime_type_from_url("a.png").unwrap(), "image/png");
        assert_eq!(detect_mime_type_from_url("a.jpeg").unwrap(), "image/jpeg");
        assert!(detect_mime_type_from_url("a.bmp").is_err());
    }
}
