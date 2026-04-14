use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_stream::StreamExt as _;

pub struct BrowserTool {
    sessions: Mutex<HashMap<String, Arc<BrowserSession>>>,
}

struct BrowserSession {
    browser: Mutex<Browser>,
    page: Page,
    handler_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn session_for(
        &self,
        ctx: &ToolContext,
    ) -> std::result::Result<Arc<BrowserSession>, String> {
        if let Some(session) = self.sessions.lock().await.get(&ctx.session_id).cloned() {
            return Ok(session);
        }

        let session = Arc::new(Self::launch_session(ctx).await?);
        self.sessions
            .lock()
            .await
            .insert(ctx.session_id.clone(), Arc::clone(&session));
        Ok(session)
    }

    async fn launch_session(ctx: &ToolContext) -> std::result::Result<BrowserSession, String> {
        let browser_cfg = &ctx.tool_config.browser;
        let mut builder = BrowserConfig::builder()
            .window_size(browser_cfg.viewport_width, browser_cfg.viewport_height)
            .request_timeout(Duration::from_secs(browser_cfg.action_timeout_secs))
            .launch_timeout(Duration::from_secs(browser_cfg.launch_timeout_secs));

        if !browser_cfg.headless {
            builder = builder.with_head();
        }
        if !browser_cfg.sandbox {
            builder = builder.no_sandbox();
        }
        if let Some(path) = &browser_cfg.executable {
            builder = builder.chrome_executable(path);
        }

        let config = builder
            .build()
            .map_err(|err| format!("failed to build browser config: {err}"))?;
        let (browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|err| format!("failed to launch browser: {err}"))?;

        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(err) = event {
                    tracing::warn!(error = %err, "browser handler exited with error");
                    break;
                }
            }
        });

        let page = browser
            .new_page("about:blank")
            .await
            .map_err(|err| format!("failed to open browser page: {err}"))?;

        Ok(BrowserSession {
            browser: Mutex::new(browser),
            page,
            handler_task: Mutex::new(Some(handler_task)),
        })
    }

    async fn close_session(&self, session_id: &str) -> std::result::Result<bool, String> {
        let session = self.sessions.lock().await.remove(session_id);
        let Some(session) = session else {
            return Ok(false);
        };

        let mut browser = session.browser.lock().await;
        browser
            .close()
            .await
            .map_err(|err| format!("failed to close browser: {err}"))?;
        let _ = browser.wait().await;
        drop(browser);

        if let Some(task) = session.handler_task.lock().await.take() {
            task.abort();
        }
        Ok(true)
    }

    async fn execute_action(
        &self,
        action: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, String> {
        match action {
            "close" => {
                let closed = self.close_session(&ctx.session_id).await?;
                Ok(ToolResult::ok(json!({ "closed": closed }).to_string()))
            }
            "wait" => {
                let session = self.session_for(ctx).await?;
                let timeout_ms = timeout_ms(args, ctx);
                if let Some(selector) = args.get("selector").and_then(|v| v.as_str()) {
                    wait_for_selector(&session.page, selector, timeout_ms).await?;
                    Ok(ToolResult::ok(
                        json!({ "ok": true, "selector": selector, "timeout_ms": timeout_ms })
                            .to_string(),
                    ))
                } else {
                    tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                    Ok(ToolResult::ok(
                        json!({ "ok": true, "slept_ms": timeout_ms }).to_string(),
                    ))
                }
            }
            "navigate" => {
                let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: url".to_string());
                };
                let session = self.session_for(ctx).await?;
                session
                    .page
                    .goto(url)
                    .await
                    .map_err(|err| format!("navigation failed: {err}"))?;
                Ok(ToolResult::ok(
                    page_summary(&session.page).await?.to_string(),
                ))
            }
            "snapshot" | "extract_text" => {
                let session = self.session_for(ctx).await?;
                let format = if action == "extract_text" {
                    "text"
                } else {
                    args.get("format")
                        .and_then(|v| v.as_str())
                        .unwrap_or("text")
                };
                let selector = args.get("selector").and_then(|v| v.as_str());
                let content = snapshot_content(
                    &session.page,
                    selector,
                    format,
                    ctx.tool_config.browser.output_max_chars,
                )
                .await?;
                let summary = page_summary(&session.page).await?;
                Ok(ToolResult::ok(
                    json!({
                        "url": summary["url"],
                        "title": summary["title"],
                        "format": format,
                        "selector": selector,
                        "content": content,
                    })
                    .to_string(),
                ))
            }
            "click" => {
                let Some(selector) = args.get("selector").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: selector".to_string());
                };
                let wait_for_navigation = args
                    .get("wait_for_navigation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let session = self.session_for(ctx).await?;
                wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await?;
                session
                    .page
                    .find_element(selector)
                    .await
                    .map_err(|err| format!("failed to find selector '{selector}': {err}"))?
                    .click()
                    .await
                    .map_err(|err| format!("click failed for '{selector}': {err}"))?;
                if wait_for_navigation {
                    session
                        .page
                        .wait_for_navigation()
                        .await
                        .map_err(|err| format!("waiting for navigation failed: {err}"))?;
                }
                Ok(ToolResult::ok(
                    json!({ "ok": true, "selector": selector, "navigated": wait_for_navigation })
                        .to_string(),
                ))
            }
            "type" => {
                let Some(selector) = args.get("selector").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: selector".to_string());
                };
                let Some(text) = args.get("text").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: text".to_string());
                };
                let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
                let session = self.session_for(ctx).await?;
                wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await?;
                if clear {
                    clear_field(&session.page, selector).await?;
                }
                session
                    .page
                    .find_element(selector)
                    .await
                    .map_err(|err| format!("failed to find selector '{selector}': {err}"))?
                    .click()
                    .await
                    .map_err(|err| format!("failed to focus selector '{selector}': {err}"))?
                    .type_str(text)
                    .await
                    .map_err(|err| format!("typing failed for '{selector}': {err}"))?;
                Ok(ToolResult::ok(
                    json!({ "ok": true, "selector": selector, "typed_chars": text.chars().count() })
                        .to_string(),
                ))
            }
            "press_key" => {
                let Some(key) = args.get("key").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: key".to_string());
                };
                let wait_for_navigation = args
                    .get("wait_for_navigation")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let session = self.session_for(ctx).await?;
                let selector = args
                    .get("selector")
                    .and_then(|v| v.as_str())
                    .unwrap_or("body");
                wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await?;
                session
                    .page
                    .find_element(selector)
                    .await
                    .map_err(|err| format!("failed to find selector '{selector}': {err}"))?
                    .click()
                    .await
                    .map_err(|err| format!("failed to focus selector '{selector}': {err}"))?
                    .press_key(key)
                    .await
                    .map_err(|err| format!("key press failed: {err}"))?;
                if wait_for_navigation {
                    session
                        .page
                        .wait_for_navigation()
                        .await
                        .map_err(|err| format!("waiting for navigation failed: {err}"))?;
                }
                Ok(ToolResult::ok(
                    json!({ "ok": true, "key": key, "navigated": wait_for_navigation }).to_string(),
                ))
            }
            other => Err(format!("unsupported browser action: {other}")),
        }
    }
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn toolset(&self) -> &str {
        "browser"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: "Automate a browser session with navigation, clicking, typing, waiting, snapshots, and key presses.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["navigate", "snapshot", "extract_text", "click", "type", "press_key", "wait", "close"]
                    },
                    "url": {"type": "string"},
                    "selector": {"type": "string"},
                    "format": {"type": "string", "enum": ["text", "html"]},
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "clear": {"type": "boolean"},
                    "wait_for_navigation": {"type": "boolean"},
                    "timeout_ms": {"type": "integer", "minimum": 1}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let Some(action) = args.get("action").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("missing required parameter: action"));
        };

        match self.execute_action(action, &args, ctx).await {
            Ok(result) => Ok(result),
            Err(message) => Ok(ToolResult::error(message)),
        }
    }
}

fn timeout_ms(args: &Value, ctx: &ToolContext) -> u64 {
    args.get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(ctx.tool_config.browser.action_timeout_secs * 1000)
}

async fn page_summary(page: &Page) -> std::result::Result<Value, String> {
    let title = page
        .get_title()
        .await
        .map_err(|err| format!("failed to read page title: {err}"))?;
    let url = page
        .url()
        .await
        .map_err(|err| format!("failed to read page url: {err}"))?
        .unwrap_or_else(|| "about:blank".to_string());
    Ok(json!({
        "url": url,
        "title": title,
    }))
}

async fn wait_for_selector(
    page: &Page,
    selector: &str,
    timeout_ms: u64,
) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if page.find_element(selector).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for selector '{selector}' after {timeout_ms}ms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn clear_field(page: &Page, selector: &str) -> std::result::Result<(), String> {
    let script = format!(
        "(() => {{ const el = document.querySelector({selector}); if (!el) throw new Error('selector not found'); if ('value' in el) el.value = ''; el.focus(); return true; }})()",
        selector = selector_literal(selector)
    );
    let _: bool = page
        .evaluate(script)
        .await
        .map_err(|err| format!("failed to clear selector '{selector}': {err}"))?
        .into_value()
        .map_err(|err| format!("failed to parse clear result: {err}"))?;
    Ok(())
}

async fn snapshot_content(
    page: &Page,
    selector: Option<&str>,
    format: &str,
    max_chars: usize,
) -> std::result::Result<String, String> {
    let script = match (selector, format) {
        (Some(selector), "html") => format!(
            "(() => {{ const el = document.querySelector({selector}); if (!el) throw new Error('selector not found'); return el.outerHTML || ''; }})()",
            selector = selector_literal(selector)
        ),
        (Some(selector), "text") => format!(
            "(() => {{ const el = document.querySelector({selector}); if (!el) throw new Error('selector not found'); return el.innerText || el.textContent || ''; }})()",
            selector = selector_literal(selector)
        ),
        (None, "html") => {
            "(() => document.documentElement?.outerHTML || document.body?.outerHTML || '')()"
                .to_string()
        }
        (None, "text") => {
            "(() => document.body?.innerText || document.body?.textContent || '')()".to_string()
        }
        (_, other) => return Err(format!("unsupported snapshot format: {other}")),
    };

    let content: String = page
        .evaluate(script)
        .await
        .map_err(|err| format!("failed to capture page snapshot: {err}"))?
        .into_value()
        .map_err(|err| format!("failed to parse page snapshot: {err}"))?;
    Ok(truncate_chars(&content, max_chars))
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let end = input
        .char_indices()
        .nth(max_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(input.len());
    input[..end].to_string()
}

fn selector_literal(selector: &str) -> String {
    serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string())
}

inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(BrowserTool::new()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_preserves_utf8_boundaries() {
        assert_eq!(truncate_chars("hello世界", 6), "hello世");
    }

    #[test]
    fn selector_literal_json_escapes() {
        assert_eq!(
            selector_literal("input[name=\"q\"]"),
            "\"input[name=\\\"q\\\"]\""
        );
    }
}
