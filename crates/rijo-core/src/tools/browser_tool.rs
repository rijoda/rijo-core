//! Browser tools — via CDP (Chrome DevTools Protocol) over WebSocket
//!
//! Minimal but real: launch/connect Chrome, navigate, snapshot (accessibility
//! tree text), click, type, scroll. No Playwright dep — raw CDP via tungstenite.
//! Requires a Chromium binary; `browser_backend` config gate.

use crate::tools::{tool_error, AsyncTool, JsonSchema, ToolMeta, ToolResult};
use crate::AppState;
use async_trait::async_trait;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub struct BrowserNavigate;
pub struct BrowserSnapshot;
pub struct BrowserClick;
pub struct BrowserType;
pub struct BrowserScroll;

#[derive(serde::Deserialize)]
struct NavArgs {
    url: String,
}

#[async_trait]
impl AsyncTool for BrowserNavigate {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_navigate",
            description: "Navigate the browser to a URL (CDP).",
            emoji: "🌐",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &NAV_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let a: NavArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => return tool_error(format!("bad args: {e}")),
        };
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }
        match cdp_navigate(&a.url).await {
            Ok(title) => json!({ "navigated": true, "title": title }).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

#[async_trait]
impl AsyncTool for BrowserSnapshot {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_snapshot",
            description: "Snapshot current page text (CDP DOM snapshot as text).",
            emoji: "📸",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &SNAP_SCHEMA
    }
    async fn call_async(&self, _args: Value, state: Arc<AppState>) -> ToolResult {
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }
        match cdp_eval("document.body.innerText").await {
            Ok(text) => json!({ "content": text }).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

#[derive(serde::Deserialize)]
struct ClickArgs {
    #[serde(rename = "ref")]
    ref_: String,
}
#[async_trait]
impl AsyncTool for BrowserClick {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_click",
            description: "Click an element by CSS selector (CDP).",
            emoji: "👆",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &CLICK_SCHEMA
    }
    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let a: ClickArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => return tool_error(format!("bad args: {e}")),
        };
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }
        let js = format!(
            "document.querySelector('{}')?.click(); 'clicked'",
            a.ref_.replace('\'', "\\'")
        );
        match cdp_eval(&js).await {
            Ok(_) => json!({"clicked": true}).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

#[derive(serde::Deserialize)]
struct TypeArgs {
    #[serde(rename = "ref")]
    ref_: String,
    text: String,
}
#[async_trait]
impl AsyncTool for BrowserType {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_type",
            description: "Type text into an element by CSS selector (CDP).",
            emoji: "⌨️",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &TYPE_SCHEMA
    }
    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let a: TypeArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => return tool_error(format!("bad args: {e}")),
        };
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }
        let js = format!(
            "(()=>{{let el=document.querySelector('{}');if(!el)return 'not found';el.focus();el.value='{}';el.dispatchEvent(new Event('input',{{bubbles:true}}));return 'typed';}})()",
            a.ref_.replace('\'', "\\'"), a.text.replace('\'', "\\'")
        );
        match cdp_eval(&js).await {
            Ok(r) => json!({"typed": true, "result": r}).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

#[derive(serde::Deserialize)]
struct ScrollArgs {
    #[serde(default)]
    direction: String,
}
#[async_trait]
impl AsyncTool for BrowserScroll {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_scroll",
            description: "Scroll page up/down (CDP).",
            emoji: "📜",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &SCROLL_SCHEMA
    }
    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let a: ScrollArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(_) => ScrollArgs {
                direction: "down".into(),
            },
        };
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }
        let delta = if a.direction == "up" { -800 } else { 800 };
        let js = format!("window.scrollBy(0,{delta}); 'scrolled'");
        match cdp_eval(&js).await {
            Ok(_) => json!({"scrolled": true}).to_string(),
            Err(e) => tool_error(e),
        }
    }
}

// ─── Browser devtools: screenshot, evaluate, console, network, tabs, … ───

pub struct BrowserDev;

#[derive(serde::Deserialize)]
struct DevArgs {
    action: String,
    #[serde(default)]
    expression: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    clear: bool,
}

#[async_trait]
impl AsyncTool for BrowserDev {
    fn meta(&self) -> &ToolMeta {
        &ToolMeta {
            name: "browser_dev",
            description: "Browser devtools (CDP): screenshot, evaluate JS, console, network, viewport, tabs, cookies, PDF.",
            emoji: "🛠️",
            toolset: "browser",
            is_async: true,
        }
    }
    fn schema(&self) -> &JsonSchema {
        &DEV_SCHEMA
    }

    async fn call_async(&self, args: Value, state: Arc<AppState>) -> ToolResult {
        let a: DevArgs = match serde_json::from_value(args) {
            Ok(v) => v,
            Err(e) => return tool_error(format!("bad args: {e}")),
        };
        if let Err(e) = ensure_cdp_async(&state.config.browser_backend).await {
            return tool_error(e);
        }

        match a.action.as_str() {
            "screenshot" => match capture("png", &state).await {
                Ok(value) => value.to_string(),
                Err(e) => tool_error(e),
            },
            "pdf" => match capture("pdf", &state).await {
                Ok(value) => value.to_string(),
                Err(e) => tool_error(e),
            },
            "evaluate" => {
                if a.expression.trim().is_empty() {
                    return tool_error("expression is required");
                }
                match cdp_eval_value(&a.expression).await {
                    Ok(value) => json!({"result": value}).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "console" => {
                let js = "JSON.stringify({hooked: !!window.__rijo_console, entries: (window.__rijo_console || []).slice(-200)})";
                match cdp_eval(js).await {
                    Ok(raw) => {
                        let parsed: Value = serde_json::from_str(&raw).unwrap_or(json!({"hooked": false, "entries": []}));
                        if a.clear && parsed["hooked"] == true {
                            let _ = cdp_eval("window.__rijo_console = []").await;
                        }
                        let mut out = json!({"entries": parsed["entries"]});
                        if parsed["hooked"] != true {
                            out["note"] = json!("console capture starts with the next browser_navigate");
                        }
                        out.to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "network" => {
                let js = "JSON.stringify(performance.getEntriesByType('resource').slice(-200).map(r => ({name: r.name, type: r.initiatorType, start: Math.round(r.startTime), duration: Math.round(r.duration), transfer: r.transferSize, encoded: r.encodedBodySize})))";
                match cdp_eval(js).await {
                    Ok(raw) => {
                        let entries: Value = serde_json::from_str(&raw).unwrap_or(json!([]));
                        if a.clear {
                            let _ = cdp_eval("performance.clearResourceTimings()").await;
                        }
                        json!({"entries": entries}).to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "viewport" => {
                if a.width == 0 || a.height == 0 {
                    return tool_error("width and height are required");
                }
                match cdp_command(
                    "Emulation.setDeviceMetricsOverride",
                    json!({"width": a.width, "height": a.height, "deviceScaleFactor": 1.0, "mobile": false}),
                )
                .await
                {
                    Ok(_) => json!({"width": a.width, "height": a.height}).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "tabs" => tabs_json().await,
            "tab_open" => {
                let url = if a.url.is_empty() { "about:blank" } else { a.url.as_str() };
                match cdp_command("Target.createTarget", json!({"url": url})).await {
                    Ok(result) => {
                        let id = result["targetId"].as_str().unwrap_or("").to_string();
                        if !id.is_empty() {
                            *ACTIVE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
                        }
                        json!({"opened": id}).to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "tab_select" => {
                if a.id.is_empty() {
                    return tool_error("id is required");
                }
                match cdp_command("Target.activateTarget", json!({"targetId": a.id})).await {
                    Ok(_) => {
                        *ACTIVE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(a.id.clone());
                        json!({"selected": a.id}).to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "tab_close" => {
                if a.id.is_empty() {
                    return tool_error("id is required");
                }
                match cdp_command("Target.closeTarget", json!({"targetId": a.id})).await {
                    Ok(_) => {
                        let mut active = ACTIVE_TARGET.lock().unwrap_or_else(|e| e.into_inner());
                        if active.as_deref() == Some(a.id.as_str()) {
                            *active = None;
                        }
                        json!({"closed": a.id}).to_string()
                    }
                    Err(e) => tool_error(e),
                }
            }
            "cookies" => {
                let result = if a.url.is_empty() {
                    cdp_command("Network.getAllCookies", json!({})).await
                } else {
                    cdp_command("Network.getCookies", json!({"urls": [a.url]})).await
                };
                match result {
                    Ok(value) => json!({"cookies": value["cookies"]}).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "cookie_set" => {
                if a.name.is_empty() || a.value.is_empty() || a.url.is_empty() {
                    return tool_error("name, value and url are required");
                }
                let mut params = json!({"name": a.name, "value": a.value, "url": a.url, "path": "/"});
                if !a.domain.is_empty() {
                    params["domain"] = json!(a.domain);
                }
                match cdp_command("Network.setCookie", params).await {
                    Ok(value) => json!({"saved": true, "success": value["success"]}).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            "cookie_delete" => {
                if a.name.is_empty() || a.url.is_empty() {
                    return tool_error("name and url are required");
                }
                match cdp_command("Network.deleteCookies", json!({"name": a.name, "url": a.url})).await {
                    Ok(_) => json!({"deleted": a.name}).to_string(),
                    Err(e) => tool_error(e),
                }
            }
            other => tool_error(format!(
                "unknown action '{other}' — use screenshot, pdf, evaluate, console, network, viewport, tabs, tab_open, tab_select, tab_close, cookies, cookie_set or cookie_delete"
            )),
        }
    }
}

async fn tabs_json() -> ToolResult {
    match cdp_page_targets().await {
        Ok(targets) => {
            let active = ACTIVE_TARGET
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let pages: Vec<Value> = targets
                .iter()
                .filter(|t| t["type"] == "page")
                .map(|t| {
                    json!({
                        "id": t["id"],
                        "title": t["title"],
                        "url": t["url"],
                        "active": active.as_deref() == t["id"].as_str(),
                    })
                })
                .collect();
            json!({"tabs": pages}).to_string()
        }
        Err(e) => tool_error(e),
    }
}

/// Capture the current page as a file under `<data_dir>/browser/`.
async fn capture(extension: &str, state: &AppState) -> Result<Value, String> {
    let (method, params) = if extension == "pdf" {
        ("Page.printToPDF", json!({"printBackground": true}))
    } else {
        (
            "Page.captureScreenshot",
            json!({"format": "png", "fromSurface": true}),
        )
    };
    let result = cdp_command(method, params).await?;
    let data = result["data"].as_str().unwrap_or("");
    if data.is_empty() {
        return Err(format!("{method} returned no data"));
    }
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("{method}: bad base64: {e}"))?;
    let dir = state.config.data_dir.join("browser");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("browser-{stamp}.{extension}"));
    std::fs::write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(json!({"path": path.display().to_string(), "bytes": bytes.len()}))
}

// ─── CDP lifecycle: own headless Chromium, autostart on demand ────────────

lazy_static::lazy_static! {
    /// Handle to the Chromium we started — so that it is terminated on shutdown
    /// and does not keep running as an orphan.
    static ref CDP_CHILD: std::sync::Mutex<Option<std::process::Child>> =
        std::sync::Mutex::new(None);
}

fn cdp_port() -> String {
    std::env::var("RIJO_CDP_PORT").unwrap_or_else(|_| "9222".into())
}

/// Quick pre-check: is anything listening on the debug port at all?
fn cdp_reachable() -> bool {
    let port: u16 = cdp_port().parse().unwrap_or(9222);
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_millis(300),
    )
    .is_ok()
}

/// The browser binary to drive, best match first.
///
/// Two lookups, because neither alone is enough:
/// - `which_bin` covers every install that is on the PATH (Linux names, and
///   Windows when the installer added it).
/// - the fixed paths cover the normal Windows case: Chrome and Edge install
///   under `C:\Program Files\...` and are usually **not** on the PATH, which is
///   why the old `which`-based probe found nothing there.
///
/// `RIJO_CHROMIUM` wins over both.
fn chromium_binary() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("RIJO_CHROMIUM") {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Some(p);
        }
    }

    const NAMES: &[&str] = &[
        "chromium",
        "chromium-browser",
        "google-chrome-stable",
        "google-chrome",
        "chrome",
        "brave",
        "brave-browser",
        "vivaldi-stable",
        "msedge",
    ];
    if let Some(found) = NAMES.iter().find_map(|bin| crate::paths::which_bin(bin)) {
        return Some(found);
    }

    const WINDOWS_PATHS: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    ];
    WINDOWS_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
}

/// Ensures that a CDP endpoint is reachable; otherwise starts our own
/// headless Chromium with its own profile (no access to the
/// user profile, no root). Gated via `browser_backend` in the config.
fn ensure_cdp(backend: &str) -> Result<(), String> {
    if backend.eq_ignore_ascii_case("none") || backend.eq_ignore_ascii_case("off") {
        return Err(format!(
            "browser backend is '{backend}' — set browser_backend: cdp in ~/.rijo/config.yaml"
        ));
    }
    if cdp_reachable() {
        return Ok(());
    }
    let bin = chromium_binary().ok_or_else(|| {
        "no Chromium/Chrome found (chrome, msedge, chromium, brave) — or set RIJO_CHROMIUM"
            .to_string()
    })?;
    let profile = crate::paths::rijo_home().join("chrome-cdp");
    let _ = std::fs::create_dir_all(&profile);

    let child = std::process::Command::new(bin)
        .arg("--headless=new")
        .arg(format!("--remote-debugging-port={}", cdp_port()))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--no-sandbox",
            "--window-size=1280,900",
        ])
        .arg("about:blank")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Chromium start failed: {e}"))?;

    if let Ok(mut guard) = CDP_CHILD.lock() {
        if let Some(mut old) = guard.take() {
            let _ = old.kill();
        }
        *guard = Some(child);
    }
    tracing::info!("[browser] headless Chromium started (port {})", cdp_port());
    // A fresh browser has no tabs from the previous session.
    *ACTIVE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = None;

    for _ in 0..50 {
        if cdp_reachable() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!(
        "Chromium started, but CDP port {} is not responding (10s)",
        cdp_port()
    ))
}

/// Async wrapper: the Chromium start blocks for up to 10s, so it runs on the
/// blocking pool instead of the runtime thread.
async fn ensure_cdp_async(backend: &str) -> Result<(), String> {
    let backend = backend.to_string();
    tokio::task::spawn_blocking(move || ensure_cdp(&backend))
        .await
        .map_err(|e| format!("browser start task: {e}"))?
}

/// Shuts down the browser we started (best effort).
pub fn shutdown_cdp() {
    if let Ok(mut guard) = CDP_CHILD.lock() {
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ─── CDP helpers ─────────────────────────────────────────────────────────

lazy_static::lazy_static! {
    /// Target id of the tab the browser tools act on. `None` = first page.
    static ref ACTIVE_TARGET: std::sync::Mutex<Option<String>> =
        std::sync::Mutex::new(None);
}

/// Console/error collector injected into every new document.
///
/// A one-shot CDP connection cannot observe `Runtime.consoleAPICalled` events,
/// so the page keeps the ring buffer itself and `browser_dev console` reads it.
const CONSOLE_HOOK: &str = r#"
(() => {
  if (window.__rijo_console) return;
  const ring = [];
  window.__rijo_console = ring;
  const record = (level, args) => {
    try {
      const text = args.map((a) => {
        if (typeof a === 'string') return a;
        try { return JSON.stringify(a); } catch { return String(a); }
      }).join(' ');
      ring.push({ level, text: String(text).slice(0, 1000), at: Date.now() });
      if (ring.length > 300) ring.shift();
    } catch {}
  };
  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    const original = console[level];
    console[level] = function (...args) {
      record(level, args);
      if (original) original.apply(console, args);
    };
  }
  window.addEventListener('error', (e) => record('error', [e.message]));
  window.addEventListener('unhandledrejection', (e) => record('error', ['unhandled rejection: ' + String(e.reason)]));
})();
"#;

async fn cdp_page_targets() -> Result<Vec<Value>, String> {
    let port = cdp_port();
    let targets: Value = reqwest::get(format!("http://127.0.0.1:{port}/json"))
        .await
        .map_err(|e| {
            format!("CDP not reachable ({e}) — start chrome with --remote-debugging-port={port}")
        })?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(targets.as_array().cloned().unwrap_or_default())
}

async fn cdp_page_ws() -> Result<String, String> {
    let targets = cdp_page_targets().await?;
    let pages: Vec<&Value> = targets.iter().filter(|t| t["type"] == "page").collect();
    let active = ACTIVE_TARGET
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let target = active
        .as_deref()
        .and_then(|id| pages.iter().find(|t| t["id"].as_str() == Some(id)).copied())
        .or_else(|| pages.first().copied())
        .ok_or("no page target")?;
    target["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "page target has no webSocketDebuggerUrl".to_string())
}

async fn cdp_command(method: &str, params: Value) -> Result<Value, String> {
    let ws_url = cdp_page_ws().await?;
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    let (mut write, mut read) = ws.split();
    let msg = json!({"id": 1, "method": method, "params": params});
    write
        .send(tokio_tungstenite::tungstenite::Message::text(
            msg.to_string(),
        ))
        .await
        .map_err(|e| e.to_string())?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(format!("CDP {method}: timeout"));
        }
        match tokio::time::timeout(left, read.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(txt)))) => {
                let value: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
                if value["id"] != 1 {
                    continue;
                }
                if let Some(error) = value["error"]["message"].as_str() {
                    return Err(format!("CDP {method}: {error}"));
                }
                return Ok(value["result"].clone());
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => return Err(format!("ws: {e}")),
            Ok(None) => return Err(format!("CDP {method}: socket closed")),
            Err(_) => return Err(format!("CDP {method}: timeout")),
        }
    }
}

async fn cdp_eval_value(js: &str) -> Result<Value, String> {
    let result = cdp_command(
        "Runtime.evaluate",
        json!({"expression": js, "returnByValue": true, "awaitPromise": true, "userGesture": true}),
    )
    .await?;
    if let Some(details) = result.get("exceptionDetails") {
        let text = details["exception"]["description"]
            .as_str()
            .or_else(|| details["text"].as_str())
            .unwrap_or("JavaScript error");
        return Err(text.to_string());
    }
    Ok(result["result"]["value"].clone())
}

async fn cdp_eval(js: &str) -> Result<String, String> {
    let value = cdp_eval_value(js).await?;
    Ok(match value {
        Value::String(text) => text,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

async fn cdp_navigate(url: &str) -> Result<String, String> {
    // Register the console collector for this and later documents; the guard
    // inside makes repeated registrations harmless.
    let _ = cdp_command(
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": CONSOLE_HOOK}),
    )
    .await;

    let result = cdp_command("Page.navigate", json!({"url": url})).await?;
    if let Some(error) = result["errorText"].as_str() {
        return Err(format!("navigate: {error}"));
    }
    // No Page.loadEventFired on a one-shot connection: give the document a
    // moment to commit and read the title. The agent can wait with evaluate.
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    Ok(cdp_eval("document.title").await.unwrap_or_default())
}
lazy_static::lazy_static! {
    static ref NAV_SCHEMA: JsonSchema = JsonSchema::new(json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}));
    static ref SNAP_SCHEMA: JsonSchema = JsonSchema::new(json!({"type":"object"}));
    static ref CLICK_SCHEMA: JsonSchema = JsonSchema::new(json!({"type":"object","properties":{"ref":{"type":"string"}},"required":["ref"]}));
    static ref TYPE_SCHEMA: JsonSchema = JsonSchema::new(json!({"type":"object","properties":{"ref":{"type":"string"},"text":{"type":"string"}},"required":["ref","text"]}));
    static ref SCROLL_SCHEMA: JsonSchema = JsonSchema::new(json!({"type":"object","properties":{"direction":{"type":"string","enum":["up","down"]}}}));
    static ref DEV_SCHEMA: JsonSchema = JsonSchema::new(json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "screenshot", "pdf", "evaluate", "console", "network",
                    "viewport", "tabs", "tab_open", "tab_select", "tab_close",
                    "cookies", "cookie_set", "cookie_delete"
                ],
                "description": "Devtools action."
            },
            "expression": {"type": "string", "description": "JavaScript for evaluate (result returned by value)."},
            "width": {"type": "integer", "description": "Viewport width for viewport."},
            "height": {"type": "integer", "description": "Viewport height for viewport."},
            "url": {"type": "string", "description": "URL for tab_open, cookies, cookie_set, cookie_delete."},
            "name": {"type": "string", "description": "Cookie name for cookie_set/cookie_delete."},
            "value": {"type": "string", "description": "Cookie value for cookie_set."},
            "domain": {"type": "string", "description": "Optional cookie domain."},
            "id": {"type": "string", "description": "Tab target id for tab_select/tab_close."},
            "clear": {"type": "boolean", "description": "Clear the console/network buffer after reading."}
        },
        "required": ["action"]
    }));
}

#[cfg(test)]
mod live_cdp_tests {
    use super::*;

    /// Ignored by default: starts a real headless Chromium and uses the network.
    /// Run with `cargo test -p rijo-core live_cdp -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "starts a real Chromium and needs network"]
    async fn navigate_evaluate_screenshot_and_tabs_work() {
        ensure_cdp_async("cdp").await.expect("start browser");

        let title = cdp_navigate("https://example.com").await.expect("navigate");
        assert!(title.to_lowercase().contains("example"), "title: {title}");

        let value = cdp_eval_value("1 + 1").await.expect("evaluate");
        assert_eq!(value, json!(2));

        let shot = cdp_command("Page.captureScreenshot", json!({"format": "png"}))
            .await
            .expect("screenshot");
        assert!(
            shot["data"].as_str().map(|data| data.len()).unwrap_or(0) > 100,
            "screenshot payload too small"
        );

        let tabs = cdp_page_targets().await.expect("tabs");
        assert!(tabs.iter().any(|tab| tab["type"] == "page"));

        shutdown_cdp();
    }
}
