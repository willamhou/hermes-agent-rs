use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::detection::{DetectionOptions, default_executable};
use chromiumoxide::page::Page;
use hermes_core::{
    error::Result,
    message::ToolResult,
    tool::{Tool, ToolContext, ToolSchema},
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

use crate::browser_handoff::{
    BrowserActionSnapshot, emit_browser_action_completed, emit_browser_action_failed,
    emit_browser_action_started,
};
use crate::process_registry::kill_process_group;
use crate::session_cleanup::{self, remove_browser_user_data_dir};

pub struct BrowserTool {
    sessions: Arc<BrowserSessions>,
}

struct BrowserSession {
    browser: Mutex<Browser>,
    browser_root_pid: Option<u32>,
    browser_process_group: Option<u32>,
    user_data_dir: PathBuf,
    page: Page,
    handler_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cleanup_registration: Mutex<Option<session_cleanup::SessionCleanupRegistration>>,
    shutdown_requested: Arc<AtomicBool>,
}

struct BrowserSessions {
    sessions: Mutex<HashMap<String, Arc<BrowserSession>>>,
}

impl BrowserSessions {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn get(&self, session_id: &str) -> Option<Arc<BrowserSession>> {
        self.sessions.lock().await.get(session_id).cloned()
    }

    async fn insert(&self, session_id: String, session: Arc<BrowserSession>) {
        self.sessions.lock().await.insert(session_id, session);
    }

    async fn close_session(
        &self,
        session_id: &str,
        unregister_cleanup: bool,
    ) -> std::result::Result<bool, String> {
        if unregister_cleanup {
            let session = self.sessions.lock().await.get(session_id).cloned();
            let Some(session) = session else {
                return Ok(false);
            };

            close_browser_session(Arc::clone(&session)).await?;

            self.sessions.lock().await.remove(session_id);
            if let Some(registration) = session.cleanup_registration.lock().await.take() {
                let _ = session_cleanup::unregister(&registration);
            }
            return Ok(true);
        }

        let session = self.sessions.lock().await.remove(session_id);
        let Some(session) = session else {
            return Ok(false);
        };
        close_browser_session(session).await?;
        Ok(true)
    }
}

impl BrowserTool {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(BrowserSessions::new()),
        }
    }

    async fn session_for(
        &self,
        ctx: &ToolContext,
    ) -> std::result::Result<Arc<BrowserSession>, String> {
        if let Some(session) = self.sessions.get(&ctx.session_id).await {
            return Ok(session);
        }

        let session = Arc::new(Self::launch_session(ctx, Arc::clone(&self.sessions)).await?);
        let sessions = Arc::clone(&self.sessions);
        let cleanup_session_id = ctx.session_id.clone();
        let durable = match session_cleanup::browser_session_durable_resource(
            session.browser_root_pid,
            session.browser_process_group,
            &session.user_data_dir,
        ) {
            Ok(durable) => durable,
            Err(err) => {
                let _ = close_browser_session(Arc::clone(&session)).await;
                return Err(err);
            }
        };
        let registration = session_cleanup::register_async_cleanup_with_durable_resource(
            &ctx.session_id,
            "browser session",
            durable,
            {
                move || {
                    let sessions = Arc::clone(&sessions);
                    let cleanup_session_id = cleanup_session_id.clone();
                    async move {
                        close_browser_session_after_registration(sessions, cleanup_session_id).await
                    }
                }
            },
        );
        *session.cleanup_registration.lock().await = Some(registration);
        self.sessions
            .insert(ctx.session_id.clone(), Arc::clone(&session))
            .await;
        Ok(session)
    }

    async fn launch_session(
        ctx: &ToolContext,
        sessions: Arc<BrowserSessions>,
    ) -> std::result::Result<BrowserSession, String> {
        let user_data_dir = browser_user_data_dir(&ctx.session_id);
        tokio::fs::create_dir_all(&user_data_dir)
            .await
            .map_err(|err| format!("failed to create browser user data dir: {err}"))?;
        let browser_cfg = &ctx.tool_config.browser;
        let (browser_executable, process_group_managed) =
            browser_launch_executable(&user_data_dir, browser_cfg.executable.as_ref()).await?;
        let mut builder = BrowserConfig::builder()
            .window_size(browser_cfg.viewport_width, browser_cfg.viewport_height)
            .request_timeout(Duration::from_secs(browser_cfg.action_timeout_secs))
            .launch_timeout(Duration::from_secs(browser_cfg.launch_timeout_secs))
            .user_data_dir(&user_data_dir)
            .chrome_executable(&browser_executable);

        if !browser_cfg.headless {
            builder = builder.with_head();
        }
        if !browser_cfg.sandbox {
            builder = builder.no_sandbox();
        }

        let config = match builder.build() {
            Ok(config) => config,
            Err(err) => {
                let _ = remove_browser_user_data_dir(&user_data_dir).await;
                return Err(format!("failed to build browser config: {err}"));
            }
        };
        let (mut browser, mut handler) = match Browser::launch(config).await {
            Ok(browser) => browser,
            Err(err) => {
                let _ = remove_browser_user_data_dir(&user_data_dir).await;
                return Err(format!("failed to launch browser: {err}"));
            }
        };
        let browser_root_pid = spawned_browser_root_pid(&mut browser);
        let browser_process_group = process_group_managed.then_some(browser_root_pid).flatten();
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let handler_shutdown_requested = Arc::clone(&shutdown_requested);
        let handler_sessions = Arc::clone(&sessions);
        let handler_session_id = ctx.session_id.clone();

        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if let Err(err) = event {
                    tracing::warn!(error = %err, "browser handler exited with error");
                    break;
                }
            }

            if !handler_shutdown_requested.load(Ordering::SeqCst) {
                tokio::spawn(async move {
                    if let Err(err) = close_browser_session_after_handler_exit(
                        handler_sessions,
                        handler_session_id,
                    )
                    .await
                    {
                        tracing::warn!(error = %err, "failed to auto-clean exited browser session");
                    }
                });
            }
        });

        let page = match browser.new_page("about:blank").await {
            Ok(page) => page,
            Err(err) => {
                let _ = browser.close().await;
                let _ = browser.wait().await;
                handler_task.abort();
                let _ = remove_browser_user_data_dir(&user_data_dir).await;
                return Err(format!("failed to open browser page: {err}"));
            }
        };

        Ok(BrowserSession {
            browser: Mutex::new(browser),
            browser_root_pid,
            browser_process_group,
            user_data_dir,
            page,
            handler_task: Mutex::new(Some(handler_task)),
            cleanup_registration: Mutex::new(None),
            shutdown_requested,
        })
    }

    async fn close_session(&self, session_id: &str) -> std::result::Result<bool, String> {
        self.sessions.close_session(session_id, true).await
    }

    async fn execute_action(
        &self,
        action: &str,
        args: &Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, String> {
        match action {
            "close" => {
                let existing_session = self.sessions.get(&ctx.session_id).await;
                let (page_url, page_title) = match existing_session.as_ref() {
                    Some(session) => page_summary_fields_best_effort(&session.page).await,
                    None => (None, None),
                };
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "close",
                    browser_action_snapshot(None, false, page_url.clone(), page_title.clone()),
                )
                .await;
                let closed = match self.close_session(&ctx.session_id).await {
                    Ok(closed) => closed,
                    Err(err) => {
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "close",
                            browser_action_snapshot(None, false, page_url, page_title),
                            err.clone(),
                        )
                        .await;
                        return Err(err);
                    }
                };
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    "close",
                    browser_action_snapshot(None, false, page_url, page_title),
                    None,
                )
                .await;
                Ok(ToolResult::ok(json!({ "closed": closed }).to_string()))
            }
            "wait" => {
                let session = self.session_for(ctx).await?;
                let timeout_ms = timeout_ms(args, ctx);
                let selector = args.get("selector").and_then(|v| v.as_str());
                let target = selector
                    .map(|selector| format!("selector:{selector}"))
                    .or_else(|| Some(format!("sleep_ms:{timeout_ms}")));
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "wait",
                    browser_action_snapshot(
                        target.clone(),
                        false,
                        page_url.clone(),
                        page_title.clone(),
                    ),
                )
                .await;
                if let Some(selector) = args.get("selector").and_then(|v| v.as_str()) {
                    if let Err(err) = wait_for_selector(&session.page, selector, timeout_ms).await {
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "wait",
                            browser_action_snapshot(target, false, failed_url, failed_title),
                            err.clone(),
                        )
                        .await;
                        return Err(err);
                    }
                    let (completed_url, completed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_completed(
                        ctx,
                        self.name(),
                        "wait",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}")),
                            false,
                            completed_url.clone(),
                            completed_title.clone(),
                        ),
                        None,
                    )
                    .await;
                    Ok(ToolResult::ok(
                        json!({
                            "ok": true,
                            "selector": selector,
                            "timeout_ms": timeout_ms,
                            "url": completed_url,
                            "title": completed_title,
                        })
                        .to_string(),
                    ))
                } else {
                    tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
                    let (completed_url, completed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_completed(
                        ctx,
                        self.name(),
                        "wait",
                        browser_action_snapshot(
                            Some(format!("sleep_ms:{timeout_ms}")),
                            false,
                            completed_url.clone(),
                            completed_title.clone(),
                        ),
                        None,
                    )
                    .await;
                    Ok(ToolResult::ok(
                        json!({
                            "ok": true,
                            "slept_ms": timeout_ms,
                            "url": completed_url,
                            "title": completed_title,
                        })
                        .to_string(),
                    ))
                }
            }
            "navigate" => {
                let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
                    return Err("missing required parameter: url".to_string());
                };
                let session = self.session_for(ctx).await?;
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "navigate",
                    browser_action_snapshot(
                        Some(format!("url:{url}")),
                        false,
                        page_url,
                        page_title,
                    ),
                )
                .await;
                if let Err(err) = session.page.goto(url).await {
                    let message = format!("navigation failed: {err}");
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "navigate",
                        browser_action_snapshot(
                            Some(format!("url:{url}")),
                            false,
                            failed_url,
                            failed_title,
                        ),
                        message.clone(),
                    )
                    .await;
                    return Err(message);
                }
                let summary = page_summary(&session.page).await?;
                let (completed_url, completed_title) = page_summary_fields_from_value(&summary);
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    "navigate",
                    browser_action_snapshot(
                        Some(format!("url:{url}")),
                        false,
                        completed_url,
                        completed_title,
                    ),
                    None,
                )
                .await;
                Ok(ToolResult::ok(summary.to_string()))
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
                let target = selector
                    .map(|selector| format!("selector:{selector}"))
                    .or_else(|| Some(format!("format:{format}")));
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    action,
                    browser_action_snapshot(target.clone(), false, page_url, page_title),
                )
                .await;
                let content = match snapshot_content(
                    &session.page,
                    selector,
                    format,
                    ctx.tool_config.browser.output_max_chars,
                )
                .await
                {
                    Ok(content) => content,
                    Err(err) => {
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            action,
                            browser_action_snapshot(target, false, failed_url, failed_title),
                            err.clone(),
                        )
                        .await;
                        return Err(err);
                    }
                };
                let summary = page_summary(&session.page).await?;
                let (completed_url, completed_title) = page_summary_fields_from_value(&summary);
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    action,
                    browser_action_snapshot(
                        selector.map(|selector| format!("selector:{selector}")),
                        false,
                        completed_url,
                        completed_title,
                    ),
                    Some(truncate_chars(&content, 2_000)),
                )
                .await;
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
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "click",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}")),
                        wait_for_navigation,
                        page_url,
                        page_title,
                    ),
                )
                .await;
                if let Err(err) =
                    wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await
                {
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "click",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}")),
                            wait_for_navigation,
                            failed_url,
                            failed_title,
                        ),
                        err.clone(),
                    )
                    .await;
                    return Err(err);
                }
                let element = match session.page.find_element(selector).await {
                    Ok(element) => element,
                    Err(err) => {
                        let message = format!("failed to find selector '{selector}': {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "click",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}")),
                                wait_for_navigation,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                };
                if let Err(err) = element.click().await {
                    let message = format!("click failed for '{selector}': {err}");
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "click",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}")),
                            wait_for_navigation,
                            failed_url,
                            failed_title,
                        ),
                        message.clone(),
                    )
                    .await;
                    return Err(message);
                }
                if wait_for_navigation {
                    if let Err(err) = session.page.wait_for_navigation().await {
                        let message = format!("waiting for navigation failed: {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "click",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}")),
                                wait_for_navigation,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                }
                let (completed_url, completed_title) =
                    page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    "click",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}")),
                        wait_for_navigation,
                        completed_url.clone(),
                        completed_title.clone(),
                    ),
                    None,
                )
                .await;
                Ok(ToolResult::ok(
                    json!({
                        "ok": true,
                        "selector": selector,
                        "navigated": wait_for_navigation,
                        "url": completed_url,
                        "title": completed_title,
                    })
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
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "type",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}")),
                        false,
                        page_url,
                        page_title,
                    ),
                )
                .await;
                if let Err(err) =
                    wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await
                {
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "type",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}")),
                            false,
                            failed_url,
                            failed_title,
                        ),
                        err.clone(),
                    )
                    .await;
                    return Err(err);
                }
                if clear {
                    if let Err(err) = clear_field(&session.page, selector).await {
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "type",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}")),
                                false,
                                None,
                                None,
                            ),
                            err.clone(),
                        )
                        .await;
                        return Err(err);
                    }
                }
                let element = match session.page.find_element(selector).await {
                    Ok(element) => element,
                    Err(err) => {
                        let message = format!("failed to find selector '{selector}': {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "type",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}")),
                                false,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                };
                let element = match element.click().await {
                    Ok(element) => element,
                    Err(err) => {
                        let message = format!("failed to focus selector '{selector}': {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "type",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}")),
                                false,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                };
                if let Err(err) = element.type_str(text).await {
                    let message = format!("typing failed for '{selector}': {err}");
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "type",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}")),
                            false,
                            failed_url,
                            failed_title,
                        ),
                        message.clone(),
                    )
                    .await;
                    return Err(message);
                }
                let (completed_url, completed_title) =
                    page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    "type",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}")),
                        false,
                        completed_url.clone(),
                        completed_title.clone(),
                    ),
                    Some(format!("typed_chars:{}", text.chars().count())),
                )
                .await;
                Ok(ToolResult::ok(
                    json!({
                        "ok": true,
                        "selector": selector,
                        "typed_chars": text.chars().count(),
                        "url": completed_url,
                        "title": completed_title,
                    })
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
                let (page_url, page_title) = page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_started(
                    ctx,
                    self.name(),
                    "press_key",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}|key:{key}")),
                        wait_for_navigation,
                        page_url,
                        page_title,
                    ),
                )
                .await;
                if let Err(err) =
                    wait_for_selector(&session.page, selector, timeout_ms(args, ctx)).await
                {
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "press_key",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}|key:{key}")),
                            wait_for_navigation,
                            failed_url,
                            failed_title,
                        ),
                        err.clone(),
                    )
                    .await;
                    return Err(err);
                }
                let element = match session.page.find_element(selector).await {
                    Ok(element) => element,
                    Err(err) => {
                        let message = format!("failed to find selector '{selector}': {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "press_key",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}|key:{key}")),
                                wait_for_navigation,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                };
                let element = match element.click().await {
                    Ok(element) => element,
                    Err(err) => {
                        let message = format!("failed to focus selector '{selector}': {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "press_key",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}|key:{key}")),
                                wait_for_navigation,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                };
                if let Err(err) = element.press_key(key).await {
                    let message = format!("key press failed: {err}");
                    let (failed_url, failed_title) =
                        page_summary_fields_best_effort(&session.page).await;
                    emit_browser_action_failed(
                        ctx,
                        self.name(),
                        "press_key",
                        browser_action_snapshot(
                            Some(format!("selector:{selector}|key:{key}")),
                            wait_for_navigation,
                            failed_url,
                            failed_title,
                        ),
                        message.clone(),
                    )
                    .await;
                    return Err(message);
                }
                if wait_for_navigation {
                    if let Err(err) = session.page.wait_for_navigation().await {
                        let message = format!("waiting for navigation failed: {err}");
                        let (failed_url, failed_title) =
                            page_summary_fields_best_effort(&session.page).await;
                        emit_browser_action_failed(
                            ctx,
                            self.name(),
                            "press_key",
                            browser_action_snapshot(
                                Some(format!("selector:{selector}|key:{key}")),
                                wait_for_navigation,
                                failed_url,
                                failed_title,
                            ),
                            message.clone(),
                        )
                        .await;
                        return Err(message);
                    }
                }
                let (completed_url, completed_title) =
                    page_summary_fields_best_effort(&session.page).await;
                emit_browser_action_completed(
                    ctx,
                    self.name(),
                    "press_key",
                    browser_action_snapshot(
                        Some(format!("selector:{selector}|key:{key}")),
                        wait_for_navigation,
                        completed_url.clone(),
                        completed_title.clone(),
                    ),
                    None,
                )
                .await;
                Ok(ToolResult::ok(
                    json!({
                        "ok": true,
                        "key": key,
                        "navigated": wait_for_navigation,
                        "url": completed_url,
                        "title": completed_title,
                    })
                    .to_string(),
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

async fn close_browser_session(session: Arc<BrowserSession>) -> std::result::Result<(), String> {
    session.shutdown_requested.store(true, Ordering::SeqCst);
    let mut close_error = None;
    let mut browser = session.browser.lock().await;
    if let Err(err) = browser.close().await {
        if let Some(process_group) = session.browser_process_group {
            if let Err(kill_err) = kill_process_group(process_group) {
                close_error = Some(format!(
                    "failed to close browser: {err}; failed to kill browser process group: {kill_err}"
                ));
            }
        } else {
            match browser.kill().await {
                Some(Ok(())) => {}
                Some(Err(kill_err)) => {
                    close_error = Some(format!(
                        "failed to close browser: {err}; failed to kill browser process: {kill_err}"
                    ));
                }
                None => {
                    close_error = Some(format!("failed to close browser: {err}"));
                }
            }
        }
    }
    let _ = browser.wait().await;
    drop(browser);

    if let Some(task) = session.handler_task.lock().await.take() {
        task.abort();
    }

    let dir_error = remove_browser_user_data_dir(&session.user_data_dir)
        .await
        .err();
    match (close_error, dir_error) {
        (Some(close_error), Some(dir_error)) => Err(format!(
            "{close_error}; failed to remove browser user data dir: {dir_error}"
        )),
        (Some(close_error), None) => Err(close_error),
        (None, Some(dir_error)) => Err(format!(
            "failed to remove browser user data dir: {dir_error}"
        )),
        (None, None) => Ok(()),
    }
}

async fn close_browser_session_after_registration(
    sessions: Arc<BrowserSessions>,
    session_id: String,
) -> std::result::Result<(), String> {
    for _ in 0..10 {
        match sessions.close_session(&session_id, false).await {
            Ok(true) => return Ok(()),
            Ok(false) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(err) => return Err(err),
        }
    }

    Err(format!(
        "browser session '{session_id}' was not ready when cleanup was requested"
    ))
}

async fn close_browser_session_after_handler_exit(
    sessions: Arc<BrowserSessions>,
    session_id: String,
) -> std::result::Result<(), String> {
    for _ in 0..10 {
        match sessions.close_session(&session_id, true).await {
            Ok(true) => return Ok(()),
            Ok(false) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

fn spawned_browser_root_pid(browser: &mut Browser) -> Option<u32> {
    browser
        .get_mut_child()
        .map(|child| child.as_mut_inner().id())
}

fn browser_user_data_dir(session_id: &str) -> PathBuf {
    let session_fragment = session_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(32)
        .collect::<String>();
    let suffix = Uuid::new_v4().simple().to_string();
    let name = if session_fragment.is_empty() {
        format!("browser-{suffix}")
    } else {
        format!("browser-{session_fragment}-{suffix}")
    };
    std::env::temp_dir()
        .join("hermes-browser-sessions")
        .join(name)
}

async fn browser_launch_executable(
    user_data_dir: &Path,
    configured_executable: Option<&PathBuf>,
) -> std::result::Result<(PathBuf, bool), String> {
    let executable = if let Some(path) = configured_executable {
        path.clone()
    } else {
        default_executable(DetectionOptions::default())
            .map_err(|err| format!("failed to detect browser executable: {err}"))?
    };

    #[cfg(unix)]
    {
        let wrapper_path = user_data_dir.join("launch-browser.sh");
        let script = format!(
            "#!/bin/sh\nexec setsid {} \"$@\"\n",
            shell_single_quote(&executable.to_string_lossy())
        );
        tokio::fs::write(&wrapper_path, script)
            .await
            .map_err(|err| format!("failed to write browser launcher wrapper: {err}"))?;
        let mut perms = tokio::fs::metadata(&wrapper_path)
            .await
            .map_err(|err| format!("failed to read browser launcher wrapper metadata: {err}"))?
            .permissions();
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o700);
        tokio::fs::set_permissions(&wrapper_path, perms)
            .await
            .map_err(|err| format!("failed to mark browser launcher wrapper executable: {err}"))?;
        Ok((wrapper_path, true))
    }

    #[cfg(not(unix))]
    {
        let _ = user_data_dir;
        Ok((executable, false))
    }
}

#[cfg(unix)]
fn shell_single_quote(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
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

fn browser_action_snapshot(
    target: Option<String>,
    wait_for_navigation: bool,
    page_url: Option<String>,
    page_title: Option<String>,
) -> BrowserActionSnapshot {
    BrowserActionSnapshot {
        target,
        wait_for_navigation,
        page_url,
        page_title,
    }
}

async fn page_summary_fields_best_effort(page: &Page) -> (Option<String>, Option<String>) {
    match page_summary(page).await {
        Ok(summary) => page_summary_fields_from_value(&summary),
        Err(_) => (None, None),
    }
}

fn page_summary_fields_from_value(summary: &Value) -> (Option<String>, Option<String>) {
    (
        summary["url"].as_str().map(ToOwned::to_owned),
        summary["title"].as_str().map(ToOwned::to_owned),
    )
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
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hermes_core::{
        stream::StreamDelta,
        tool::{
            ApprovalRequest, BrowserToolConfig, FileToolConfig, TerminalToolConfig, Tool,
            ToolConfig, ToolContext,
        },
    };
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    use super::*;
    use crate::session_cleanup::{self, DurableCleanupRecorder, DurableCleanupResource};

    #[derive(Default)]
    struct MockRecorder {
        registered: Mutex<Vec<(String, u64, DurableCleanupResource)>>,
        unregistered: Mutex<Vec<(String, u64)>>,
    }

    #[async_trait]
    impl DurableCleanupRecorder for MockRecorder {
        async fn register(
            &self,
            session_id: &str,
            entry_id: u64,
            resource: DurableCleanupResource,
        ) -> std::result::Result<(), String> {
            self.registered
                .lock()
                .expect("mock recorder registered lock poisoned")
                .push((session_id.to_string(), entry_id, resource));
            Ok(())
        }

        async fn unregister(
            &self,
            session_id: &str,
            entry_id: u64,
        ) -> std::result::Result<(), String> {
            self.unregistered
                .lock()
                .expect("mock recorder unregistered lock poisoned")
                .push((session_id.to_string(), entry_id));
            Ok(())
        }
    }

    struct RecorderGuard(Option<Arc<dyn DurableCleanupRecorder>>);

    impl RecorderGuard {
        fn install(recorder: Arc<dyn DurableCleanupRecorder>) -> Self {
            Self(session_cleanup::replace_durable_cleanup_recorder(Some(
                recorder,
            )))
        }
    }

    impl Drop for RecorderGuard {
        fn drop(&mut self) {
            let _ = session_cleanup::replace_durable_cleanup_recorder(self.0.take());
        }
    }

    fn make_test_ctx_with_delta(
        workspace: &Path,
        session_id: &str,
    ) -> (ToolContext, mpsc::Receiver<StreamDelta>) {
        let (approval_tx, _) = mpsc::channel::<ApprovalRequest>(1);
        let (delta_tx, delta_rx) = mpsc::channel::<StreamDelta>(16);
        let browser_executable = std::env::var_os("CHROME").map(PathBuf::from);
        (
            ToolContext {
                session_id: session_id.to_string(),
                working_dir: workspace.to_path_buf(),
                approval_tx,
                delta_tx,
                execution_observer: None,
                tool_config: Arc::new(ToolConfig {
                    terminal: TerminalToolConfig::default(),
                    file: FileToolConfig::default(),
                    browser: BrowserToolConfig {
                        sandbox: false,
                        executable: browser_executable,
                        ..BrowserToolConfig::default()
                    },
                    workspace_root: workspace.to_path_buf(),
                }),
                memory: None,
                aux_provider: None,
                skills: None,
                delegation_depth: 0,
                clarify_tx: None,
            },
            delta_rx,
        )
    }

    fn make_test_ctx(workspace: &Path, session_id: &str) -> ToolContext {
        make_test_ctx_with_delta(workspace, session_id).0
    }

    fn browser_env_unavailable(message: &str) -> bool {
        message.contains("failed to detect browser executable")
    }

    async fn navigate_or_skip(tool: &BrowserTool, ctx: &ToolContext, url: &str) -> bool {
        let result = tool
            .execute(serde_json::json!({ "action": "navigate", "url": url }), ctx)
            .await
            .unwrap();
        if result.is_error && browser_env_unavailable(&result.content) {
            eprintln!("skipping live browser test: {}", result.content);
            return false;
        }
        assert!(!result.is_error, "{}", result.content);
        true
    }

    async fn wait_for_session_unregistered(recorder: &MockRecorder, session_id: &str) -> bool {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let unregistered = recorder
                    .unregistered
                    .lock()
                    .expect("mock recorder unregistered lock poisoned")
                    .clone();
                if unregistered
                    .iter()
                    .any(|(unregistered_session_id, _)| unregistered_session_id == session_id)
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_ok()
    }

    fn fixture_url(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        format!("file://{}", path.display())
    }

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

    #[tokio::test]
    async fn live_browser_close_unregisters_manifest_and_removes_user_data_dir() {
        let _lock = session_cleanup::DURABLE_RECORDER_TEST_LOCK.lock().await;
        let recorder = Arc::new(MockRecorder::default());
        let _guard = RecorderGuard::install(recorder.clone());

        let workspace = TempDir::new().unwrap();
        let session_id = format!("browser-live-close-{}", Uuid::new_v4().simple());
        let ctx = make_test_ctx(workspace.path(), &session_id);
        let html_path = workspace.path().join("index.html");
        std::fs::write(
            &html_path,
            "<html><body><h1>Hello browser</h1></body></html>",
        )
        .unwrap();
        let url = format!("file://{}", html_path.display());
        let tool = BrowserTool::new();

        if !navigate_or_skip(&tool, &ctx, &url).await {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        let registered = recorder
            .registered
            .lock()
            .expect("mock recorder registered lock poisoned")
            .clone();
        let session_registered: Vec<_> = registered
            .into_iter()
            .filter(|(registered_session_id, _, _)| registered_session_id == &session_id)
            .collect();
        assert_eq!(session_registered.len(), 1);
        assert_eq!(
            session_registered[0].2.kind,
            session_cleanup::DurableCleanupResourceKind::BrowserSession
        );
        let target: serde_json::Value =
            serde_json::from_str(&session_registered[0].2.target_value).unwrap();
        let user_data_dir = PathBuf::from(target["user_data_dir"].as_str().unwrap());
        assert!(user_data_dir.exists());
        #[cfg(unix)]
        assert!(target["process_group"].as_u64().is_some());

        let result = tool
            .execute(serde_json::json!({ "action": "close" }), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error, "{}", result.content);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!user_data_dir.exists());
        let unregistered = recorder
            .unregistered
            .lock()
            .expect("mock recorder unregistered lock poisoned")
            .clone();
        assert!(
            unregistered
                .iter()
                .any(|(unregistered_session_id, _)| unregistered_session_id == &session_id)
        );
    }

    #[tokio::test]
    async fn live_browser_cleanup_session_reclaims_manifest_and_user_data_dir() {
        let _lock = session_cleanup::DURABLE_RECORDER_TEST_LOCK.lock().await;
        let recorder = Arc::new(MockRecorder::default());
        let _guard = RecorderGuard::install(recorder.clone());

        let workspace = TempDir::new().unwrap();
        let session_id = format!("browser-live-cleanup-{}", Uuid::new_v4().simple());
        let ctx = make_test_ctx(workspace.path(), &session_id);
        let html_path = workspace.path().join("index.html");
        std::fs::write(&html_path, "<html><body><p>Cleanup me</p></body></html>").unwrap();
        let url = format!("file://{}", html_path.display());
        let tool = BrowserTool::new();

        if !navigate_or_skip(&tool, &ctx, &url).await {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        let registered = recorder
            .registered
            .lock()
            .expect("mock recorder registered lock poisoned")
            .clone();
        let session_registered: Vec<_> = registered
            .into_iter()
            .filter(|(registered_session_id, _, _)| registered_session_id == &session_id)
            .collect();
        assert_eq!(session_registered.len(), 1);
        let target: serde_json::Value =
            serde_json::from_str(&session_registered[0].2.target_value).unwrap();
        let user_data_dir = PathBuf::from(target["user_data_dir"].as_str().unwrap());
        assert!(user_data_dir.exists());

        let summary = session_cleanup::cleanup_session(&session_id).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty(), "{:?}", summary.failures);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!user_data_dir.exists());
    }

    #[tokio::test]
    async fn live_browser_interaction_flow_updates_page_state() {
        let _lock = session_cleanup::DURABLE_RECORDER_TEST_LOCK.lock().await;
        let workspace = TempDir::new().unwrap();
        let session_id = format!("browser-live-flow-{}", Uuid::new_v4().simple());
        let ctx = make_test_ctx(workspace.path(), &session_id);
        let tool = BrowserTool::new();
        let url = fixture_url("browser_interaction.html");

        if !navigate_or_skip(&tool, &ctx, &url).await {
            return;
        }

        let type_result = tool
            .execute(
                serde_json::json!({
                    "action": "type",
                    "selector": "#name-input",
                    "text": "Hermes",
                    "clear": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!type_result.is_error, "{}", type_result.content);

        let click_result = tool
            .execute(
                serde_json::json!({
                    "action": "click",
                    "selector": "#apply-button"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!click_result.is_error, "{}", click_result.content);

        let extract_result = tool
            .execute(
                serde_json::json!({
                    "action": "extract_text",
                    "selector": "#result"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!extract_result.is_error, "{}", extract_result.content);
        let extract_payload: serde_json::Value =
            serde_json::from_str(&extract_result.content).unwrap();
        assert_eq!(extract_payload["content"], "Hello, Hermes!");

        let type_again_result = tool
            .execute(
                serde_json::json!({
                    "action": "type",
                    "selector": "#name-input",
                    "text": "Agent",
                    "clear": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!type_again_result.is_error, "{}", type_again_result.content);

        let press_result = tool
            .execute(
                serde_json::json!({
                    "action": "press_key",
                    "selector": "#name-input",
                    "key": "Enter"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!press_result.is_error, "{}", press_result.content);

        let submitted_result = tool
            .execute(
                serde_json::json!({
                    "action": "extract_text",
                    "selector": "#result"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!submitted_result.is_error, "{}", submitted_result.content);
        let submitted_payload: serde_json::Value =
            serde_json::from_str(&submitted_result.content).unwrap();
        assert_eq!(submitted_payload["content"], "Submitted: Agent");

        let delayed_click_result = tool
            .execute(
                serde_json::json!({
                    "action": "click",
                    "selector": "#delayed-button"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            !delayed_click_result.is_error,
            "{}",
            delayed_click_result.content
        );

        let wait_result = tool
            .execute(
                serde_json::json!({
                    "action": "wait",
                    "selector": "#delayed-result",
                    "timeout_ms": 1000
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!wait_result.is_error, "{}", wait_result.content);

        let status_result = tool
            .execute(
                serde_json::json!({
                    "action": "extract_text",
                    "selector": "#status"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!status_result.is_error, "{}", status_result.content);
        let status_payload: serde_json::Value =
            serde_json::from_str(&status_result.content).unwrap();
        assert_eq!(status_payload["content"], "Ready");
    }

    #[tokio::test]
    async fn live_browser_emits_action_handoff_events() {
        let _lock = session_cleanup::DURABLE_RECORDER_TEST_LOCK.lock().await;
        let workspace = TempDir::new().unwrap();
        let session_id = format!("browser-live-events-{}", Uuid::new_v4().simple());
        let (ctx, mut delta_rx) = make_test_ctx_with_delta(workspace.path(), &session_id);
        let tool = BrowserTool::new();
        let url = fixture_url("browser_interaction.html");

        if !navigate_or_skip(&tool, &ctx, &url).await {
            return;
        }

        let extract_result = tool
            .execute(
                serde_json::json!({
                    "action": "extract_text",
                    "selector": "#status"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!extract_result.is_error, "{}", extract_result.content);

        let mut tool_events = Vec::new();
        while let Ok(delta) = delta_rx.try_recv() {
            if let StreamDelta::ToolEvent {
                kind,
                tool,
                metadata,
                ..
            } = delta
            {
                tool_events.push((kind, tool, metadata));
            }
        }

        assert!(tool_events.iter().any(|(kind, tool, metadata)| {
            kind == "tool.browser_action_started"
                && tool == "browser"
                && metadata
                    .as_ref()
                    .and_then(|value| value.get("action"))
                    .and_then(|value| value.as_str())
                    == Some("navigate")
        }));
        assert!(tool_events.iter().any(|(kind, tool, metadata)| {
            kind == "tool.browser_action_completed"
                && tool == "browser"
                && metadata
                    .as_ref()
                    .and_then(|value| value.get("action"))
                    .and_then(|value| value.as_str())
                    == Some("extract_text")
                && metadata
                    .as_ref()
                    .and_then(|value| value.get("output_preview"))
                    .and_then(|value| value.as_str())
                    .map(|preview| preview.contains("Ready"))
                    .unwrap_or(false)
        }));
    }

    #[tokio::test]
    async fn live_browser_unexpected_exit_reclaims_manifest_and_user_data_dir() {
        let _lock = session_cleanup::DURABLE_RECORDER_TEST_LOCK.lock().await;
        let recorder = Arc::new(MockRecorder::default());
        let _guard = RecorderGuard::install(recorder.clone());

        let workspace = TempDir::new().unwrap();
        let session_id = format!("browser-live-exit-{}", Uuid::new_v4().simple());
        let ctx = make_test_ctx(workspace.path(), &session_id);
        let tool = BrowserTool::new();
        let url = fixture_url("browser_interaction.html");

        if !navigate_or_skip(&tool, &ctx, &url).await {
            return;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        let registered = recorder
            .registered
            .lock()
            .expect("mock recorder registered lock poisoned")
            .clone();
        let session_registered: Vec<_> = registered
            .into_iter()
            .filter(|(registered_session_id, _, _)| registered_session_id == &session_id)
            .collect();
        assert_eq!(session_registered.len(), 1);
        let target: serde_json::Value =
            serde_json::from_str(&session_registered[0].2.target_value).unwrap();
        let user_data_dir = PathBuf::from(target["user_data_dir"].as_str().unwrap());
        assert!(user_data_dir.exists());

        if let Some(process_group) = target["process_group"].as_u64() {
            kill_process_group(process_group as u32).unwrap();
        } else if let Some(root_pid) = target["root_pid"].as_u64() {
            crate::process_registry::kill_process(root_pid as u32).unwrap();
        } else {
            panic!("live browser manifest did not include process_group or root_pid");
        }

        assert!(wait_for_session_unregistered(&recorder, &session_id).await);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!user_data_dir.exists());
        assert!(tool.sessions.get(&session_id).await.is_none());
    }
}
