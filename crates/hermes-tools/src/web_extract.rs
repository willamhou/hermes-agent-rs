use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

use crate::net_utils::{build_safe_client, fetch_with_redirects};

pub struct WebExtractTool;

static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b.*?>.*?</script>").unwrap());
static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style\b.*?>.*?</style>").unwrap());
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<[^>]+>").unwrap());
static TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
static WHITESPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

#[async_trait]
impl Tool for WebExtractTool {
    fn name(&self) -> &str {
        "web_extract"
    }

    fn toolset(&self) -> &str {
        "web"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Fetch and extract readable text from a web page.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: url"));
        };

        let client = match build_safe_client() {
            Ok(client) => client,
            Err(e) => return Ok(ToolResult::error(format!("failed to build client: {e}"))),
        };

        let (final_url, response) = match fetch_with_redirects(&client, url, 5).await {
            Ok(result) => result,
            Err(e) => return Ok(ToolResult::error(e)),
        };

        if !response.status().is_success() {
            return Ok(ToolResult::error(format!(
                "fetch failed with status {}",
                response.status()
            )));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "failed to read response body: {e}"
                )));
            }
        };

        let (title, content) = if content_type.starts_with("text/html") {
            let title = extract_title(&body);
            let content = truncate_chars(&html_to_text(&body), 50_000);
            (title, content)
        } else if content_type.starts_with("text/plain") || content_type.is_empty() {
            (String::new(), truncate_chars(&body, 50_000))
        } else {
            return Ok(ToolResult::error("unsupported content type"));
        };

        Ok(ToolResult::ok(
            json!({
                "url": final_url.as_str(),
                "title": title,
                "content": content
            })
            .to_string(),
        ))
    }
}

fn extract_title(html: &str) -> String {
    TITLE_RE
        .captures(html)
        .and_then(|captures| captures.get(1))
        .map(|value| html_decode(value.as_str()).trim().to_string())
        .unwrap_or_default()
}

fn html_to_text(html: &str) -> String {
    let without_scripts = SCRIPT_RE.replace_all(html, " ");
    let without_styles = STYLE_RE.replace_all(&without_scripts, " ");
    let without_tags = TAG_RE.replace_all(&without_styles, " ");
    let decoded = html_decode(&without_tags);
    WHITESPACE_RE.replace_all(&decoded, " ").trim().to_string()
}

fn html_decode(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let end = input
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(input.len());
    input[..end].to_string()
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(WebExtractTool) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags() {
        let text = html_to_text(
            "<html><head><title>Hello</title></head><body><script>x</script><p>Hello &amp; world</p></body></html>",
        );
        assert!(text.contains("Hello & world"));
        assert!(!text.contains("<p>"));
    }
}
