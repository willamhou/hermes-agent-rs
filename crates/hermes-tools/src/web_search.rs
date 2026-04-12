use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};

pub struct WebSearchTool;

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TavilyResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn toolset(&self) -> &str {
        "web"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        std::env::var("TAVILY_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Search the public web for recent information.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: query"));
        };
        let api_key = match std::env::var("TAVILY_API_KEY") {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                return Ok(ToolResult::error(
                    "web_search unavailable: TAVILY_API_KEY is not set",
                ));
            }
        };

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(e) => return Ok(ToolResult::error(format!("failed to build client: {e}"))),
        };

        let response = match client
            .post("https://api.tavily.com/search")
            .json(&json!({
                "api_key": api_key,
                "query": query,
                "max_results": 5
            }))
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => return Ok(ToolResult::error(format!("web search failed: {e}"))),
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolResult::error(format!(
                "web search API error ({status}): {body}"
            )));
        }

        let parsed: TavilyResponse = match response.json().await {
            Ok(parsed) => parsed,
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "failed to parse web search response: {e}"
                )));
            }
        };

        let formatted = parsed
            .results
            .iter()
            .map(|item| format!("{} — {}\n{}", item.title, item.url, item.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(ToolResult::ok(
            json!({
                "query": query,
                "results": parsed.results,
                "formatted": formatted
            })
            .to_string(),
        ))
    }
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(WebSearchTool) }
}
