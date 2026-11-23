//! The HTTP layer: login, the PWA shell, and the JSON API the page uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::Router;
use futures_util::stream;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::agentchat;
use crate::cmux::{revisions_from, screen_text, value_text, CmuxClient, CmuxError, Result};
use crate::events::{EventHub, SidebarCache};
use crate::net::{constant_time_eq, icon_png, netloc, shell_quote};
use crate::panels::{self, SessionFiles};
use crate::push::Push;
use crate::secrets::{SecretRequests, MAX_SECRET_BYTES};

pub const COOKIE_NAME: &str = "psycommux_token";
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_SCREEN_LINES: i64 = 2000;
/// Panes a browser may watch in one /api/screens long-poll.
const MAX_SCREENS: usize = 12;
const CHAT_POLL: Duration = Duration::from_secs(25);
const CHAT_CHECK: Duration = Duration::from_millis(400);
/// Agents whose transcripts agentchat can read.
const CHAT_AGENTS: [&str; 1] = ["claude"];
const BROWSER_KEYS: [&str; 10] = [
    "Enter",
    "Tab",
    "Escape",
    "Backspace",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "PageUp",
    "PageDown",
];
const LEASE_SECONDS: u64 = 20;
const CSP: &str =
    "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https:";

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const APP_CSS: &str = include_str!("../static/app.css");
const SW_JS: &str = include_str!("../static/sw.js");

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

pub fn abbreviate_home(path: Option<&str>) -> Option<String> {
    let path = path?;
    let home = home();
    if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) {
        return Some(format!("~{}", &path[home.len()..]));
    }
    Some(path.to_string())
}

// ---------------------------------------------------------------------------
// Browser viewports
// ---------------------------------------------------------------------------

/// Lays out browser panels at the viewer's size while psycommux shows them.
///
/// A browser panel that is not on the Mac's screen has no layout (innerWidth
/// is 0), so it can be neither screenshotted fresh nor tapped.
/// browser.viewport.set gives it one, at the phone's size; the Mac shows that
/// panel scaled to fit until it is reset. Each viewer renews a lease with every
/// screenshot; a reaper resets panels nobody has looked at for LEASE_SECONDS,
/// so a closed phone does not leave the Mac's browser emulating it.
pub struct BrowserViewports {
    client: Arc<CmuxClient>,
    leases: Mutex<HashMap<String, (i64, i64, Instant)>>,
}

impl BrowserViewports {
    pub fn new(client: Arc<CmuxClient>) -> Arc<Self> {
        let this = Arc::new(Self {
            client,
            leases: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&this);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(this) = weak.upgrade() else { return };
                let stale: Vec<String> = {
                    let leases = this.leases.lock().unwrap();
                    leases
                        .iter()
                        .filter(|(_, (_, _, at))| at.elapsed() > Duration::from_secs(LEASE_SECONDS))
                        .map(|(sid, _)| sid.clone())
                        .collect()
                };
                for sid in stale {
                    this.release(&sid).await;
                }
            }
        });
        this
    }

    pub async fn hold(&self, surface_id: &str, width: i64, height: i64) -> Result<()> {
        let (width, height) = (width.clamp(240, 2560), height.clamp(240, 2560));
        let current = self
            .leases
            .lock()
            .unwrap()
            .insert(surface_id.to_string(), (width, height, Instant::now()));
        if current.is_none_or(|(w, h, _)| (w, h) != (width, height)) {
            self.client
                .call(
                    "browser.viewport.set",
                    json!({"surface_id": surface_id, "width": width, "height": height}),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn release(&self, surface_id: &str) {
        let held = self.leases.lock().unwrap().remove(surface_id);
        if held.is_some() {
            let _ = self
                .client
                .call(
                    "browser.viewport.set",
                    json!({"surface_id": surface_id, "reset": true}),
                )
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Agent sessions
// ---------------------------------------------------------------------------

/// Coding-agent sessions cmux tracks, from `chat.sessions.dump`.
///
/// Each record ties a session to its terminal (surface_id), state and
/// transcript path. Transcript paths stay on the server; the browser only
/// names sessions by id, and only ids cmux reports are accepted.
pub struct AgentSessions {
    client: Arc<CmuxClient>,
    max_age: Duration,
    cache: tokio::sync::Mutex<SessionCache>,
}

/// When the sessions were last fetched, and the records by session id.
type SessionCache = (Option<Instant>, Arc<HashMap<String, Value>>);

fn state_of(record: &Value) -> String {
    agentchat::session_state(record.get("state").and_then(Value::as_str).unwrap_or(""))
}

impl AgentSessions {
    pub fn new(client: Arc<CmuxClient>, max_age: Duration) -> Self {
        Self {
            client,
            max_age,
            cache: tokio::sync::Mutex::new((None, Arc::new(HashMap::new()))),
        }
    }

    pub async fn all(&self) -> Arc<HashMap<String, Value>> {
        let mut cache = self.cache.lock().await;
        if cache.0.is_some_and(|at| at.elapsed() < self.max_age) {
            return Arc::clone(&cache.1);
        }
        // Older cmux, or the chat service is off: no sessions.
        let result = self
            .client
            .call("chat.sessions.dump", json!({}))
            .await
            .unwrap_or(Value::Null);
        let sessions: HashMap<String, Value> = result
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let id = s
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())?;
                Some((id.to_string(), s.clone()))
            })
            .collect();
        *cache = (Some(Instant::now()), Arc::new(sessions));
        Arc::clone(&cache.1)
    }

    pub async fn get(&self, session_id: &str) -> Result<Value> {
        self.all()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| CmuxError::new("not_found", "no such agent session"))
    }

    pub async fn transcript(&self, session_id: &str) -> Result<(Value, PathBuf)> {
        let record = self.get(session_id).await?;
        let path = PathBuf::from(
            record
                .get("transcript_path")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        let agent = record.get("agent").and_then(Value::as_str).unwrap_or("");
        if !CHAT_AGENTS.contains(&agent) || !path.is_file() {
            return Err(CmuxError::new(
                "unsupported",
                "no readable transcript for this session; use the terminal",
            ));
        }
        Ok((record, path))
    }

    pub fn public(record: &Value) -> Value {
        let path = record
            .get("transcript_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        let agent = record.get("agent").and_then(Value::as_str).unwrap_or("");
        json!({
            "session_id": record.get("session_id").cloned().unwrap_or(Value::Null),
            "agent": record.get("agent").cloned().unwrap_or(Value::Null),
            "state": state_of(record),
            "chat": CHAT_AGENTS.contains(&agent) && !path.is_empty() && std::path::Path::new(path).is_file(),
        })
    }

    /// The current session of each terminal: live ones beat ended ones, then the latest.
    pub async fn by_surface(&self) -> HashMap<String, Value> {
        let mut best: HashMap<String, ((bool, f64), Value)> = HashMap::new();
        for record in self.all().await.values() {
            let surface = record
                .get("surface_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_uppercase();
            if surface.is_empty() {
                continue;
            }
            let rank = (
                state_of(record) != "ended",
                record
                    .get("last_activity")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
            );
            if best.get(&surface).is_none_or(|(r, _)| rank > *r) {
                best.insert(surface, (rank, record.clone()));
            }
        }
        best.into_iter()
            .map(|(k, (_, record))| (k, Self::public(&record)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The workspace tree
// ---------------------------------------------------------------------------

fn field(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

fn or_value(a: Value, b: Value) -> Value {
    if is_falsy(&a) {
        b
    } else {
        a
    }
}

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn items<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// Windows, workspaces, split layouts, panes, tabs and sidebar rows.
///
/// system.tree carries the layout and panes; extension.sidebar.snapshot
/// carries what a cmux sidebar row shows (unread count, latest notification,
/// description, branch, ports); the SidebarCache adds status pills and logs.
pub async fn build_tree(
    client: &CmuxClient,
    sidebar: Option<&Arc<SidebarCache>>,
    agents: Option<&AgentSessions>,
) -> Result<Value> {
    let snapshot = client.call("system.tree", json!({})).await?;
    let agent_by_surface = match agents {
        Some(agents) => agents.by_surface().await,
        None => HashMap::new(),
    };
    let mut out = Vec::new();
    let mut all_ids = Vec::new();
    for win in items(&snapshot, "windows") {
        let mut rows: HashMap<String, Value> = HashMap::new();
        if let Ok(listed) = client
            .call(
                "extension.sidebar.snapshot",
                json!({"window_id": field(win, "id")}),
            )
            .await
        {
            for row in items(&listed, "workspaces") {
                rows.insert(value_text(&field(row, "id")), row.clone());
            }
        }
        let mut workspaces = Vec::new();
        for ws in items(win, "workspaces") {
            let ws_id = value_text(&field(ws, "id"));
            all_ids.push(ws_id.clone());
            let row = rows.get(&ws_id).cloned().unwrap_or(json!({}));
            let panes: Vec<Value> = items(ws, "panes")
                .map(|pane| {
                    let surfaces: Vec<Value> = items(pane, "surfaces")
                        .map(|surf| {
                            let id = value_text(&field(surf, "id")).to_uppercase();
                            json!({
                                "id": field(surf, "id"),
                                "ref": field(surf, "ref"),
                                "type": field(surf, "type"),
                                "title": field(surf, "title"),
                                "url": field(surf, "url"),
                                "agent": agent_by_surface.get(&id).cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect();
                    json!({
                        "id": field(pane, "id"),
                        "ref": field(pane, "ref"),
                        "focused": field(pane, "focused"),
                        "selected_surface_id": field(pane, "selected_surface_id"),
                        "surfaces": surfaces,
                    })
                })
                .collect();
            let pinned = row
                .get("pinned")
                .cloned()
                .unwrap_or_else(|| field(ws, "pinned"));
            workspaces.push(json!({
                "id": field(ws, "id"),
                "ref": field(ws, "ref"),
                "title": field(ws, "title"),
                "selected": field(ws, "selected"),
                "pinned": pinned,
                "description": field(&row, "description"),
                "custom_color": field(&row, "custom_color"),
                "unread_count": or_value(field(&row, "unread_count"), json!(0)),
                "latest_notification_text": field(&row, "latest_notification_text"),
                "latest_message": or_value(field(&row, "latest_conversation_message"), field(&row, "latest_submitted_message")),
                "directory": abbreviate_home(row.get("current_directory").and_then(Value::as_str)),
                "branch": field(&row, "branch_summary"),
                "git_dirty": items(&row, "git_branches").any(|b| !is_falsy(&field(b, "dirty"))),
                "pull_requests": or_value(field(&row, "pull_request_urls"), json!([])),
                "remote": remote_line(&row),
                "ports": or_value(field(&row, "listening_ports"), json!([])),
                "sidebar": sidebar.and_then(|s| s.get(&ws_id)),
                "layout": field(ws, "layout"),
                "panes": panes,
            }));
        }
        out.push(
            json!({"id": field(win, "id"), "ref": field(win, "ref"), "workspaces": workspaces}),
        );
    }
    if let Some(sidebar) = sidebar {
        sidebar.ensure(&all_ids);
    }
    Ok(json!({"windows": out, "sidebar_settings": sidebar_settings().await}))
}

/// The SSH line of a remote workspace's row: where it connects, and how it is doing.
fn remote_line(row: &Value) -> Value {
    let remote = field(row, "remote");
    let target = field(row, "remote_display_target");
    if is_falsy(&field(&remote, "enabled")) || target.as_str().is_none_or(str::is_empty) {
        return Value::Null;
    }
    json!({"target": target, "state": field(row, "remote_connection_state")})
}

/// Which details the Mac's sidebar shows, from cmux's settings (Settings >
/// Sidebar, or the `sidebar` block of cmux.json, which cmux keeps in its
/// defaults). Read at most every 30 seconds.
const SIDEBAR_SETTINGS: [(&str, &str); 15] = [
    ("hideAllDetails", "sidebarHideAllDetails"),
    (
        "showWorkspaceDescription",
        "sidebarShowWorkspaceDescription",
    ),
    ("showNotificationMessage", "sidebarShowNotificationMessage"),
    (
        "notificationMessageLineLimit",
        "sidebarNotificationMessageLineLimit",
    ),
    ("showBranchDirectory", "sidebarShowBranchDirectory"),
    ("stackBranchDirectory", "sidebarBranchDirectoryStacked"),
    ("pathLastSegmentOnly", "sidebarPathLastSegmentOnly"),
    ("showPullRequests", "sidebarShowPullRequest"),
    ("showSSH", "sidebarShowSSH"),
    ("showPorts", "sidebarShowPorts"),
    ("showLog", "sidebarShowLog"),
    ("showProgress", "sidebarShowProgress"),
    ("showAgentActivity", "sidebarShowAgentActivity"),
    ("showCustomMetadata", "sidebarShowStatusPills"),
    ("wrapWorkspaceTitles", "sidebarWrapWorkspaceTitles"),
];

async fn sidebar_settings() -> Value {
    static CACHE: Mutex<Option<(std::time::Instant, Value)>> = Mutex::new(None);
    if let Some((at, value)) = CACHE.lock().unwrap().as_ref()
        && at.elapsed() < Duration::from_secs(30)
    {
        return value.clone();
    }
    let plist = tokio::process::Command::new("defaults")
        .args(["export", "com.cmuxterm.app", "-"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let value = parse_sidebar_settings(&plist);
    *CACHE.lock().unwrap() = Some((std::time::Instant::now(), value.clone()));
    value
}

/// Settings the plist sets, under their cmux.json names; unset ones are left
/// out, and the page uses cmux's defaults for them.
pub fn parse_sidebar_settings(plist: &str) -> Value {
    let mut out = Map::new();
    for (name, key) in SIDEBAR_SETTINGS {
        let re = regex::Regex::new(&format!(
            r"<key>{key}</key>\s*<(true|false|integer|real)\s*/?>(?:([^<]*)</(?:integer|real)>)?"
        ))
        .unwrap();
        let Some(caps) = re.captures(plist) else {
            continue;
        };
        let value = match &caps[1] {
            "true" => json!(true),
            "false" => json!(false),
            _ => caps
                .get(2)
                .and_then(|n| n.as_str().trim().parse::<f64>().ok())
                .map(|n| json!(n as i64))
                .unwrap_or(Value::Null),
        };
        out.insert(name.into(), value);
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// State and routing
// ---------------------------------------------------------------------------

pub struct AppState {
    pub client: Arc<CmuxClient>,
    pub hub: Arc<EventHub>,
    pub agents: AgentSessions,
    pub files: SessionFiles,
    pub viewports: Arc<BrowserViewports>,
    /// Secrets `psycommux secret` waits for, answered from the phone.
    pub secrets: Arc<SecretRequests>,
    /// Web Push subscriptions of the phones' Home Screen apps.
    pub push: Arc<Push>,
    pub token: String,
    pub allowed_origins: Vec<String>,
    pub access_log: bool,
    /// Serving HTTPS ourselves: the login cookie gets `Secure`.
    pub tls: bool,
    /// Where files sent from the phone are kept for agents to read.
    pub uploads: PathBuf,
}

impl AppState {
    pub fn new(client: Arc<CmuxClient>, hub: Arc<EventHub>, token: impl Into<String>) -> Self {
        Self {
            agents: AgentSessions::new(Arc::clone(&client), Duration::from_secs(1)),
            files: SessionFiles::new(panels::default_session_glob()),
            viewports: BrowserViewports::new(Arc::clone(&client)),
            secrets: SecretRequests::new(Arc::clone(&client), Arc::clone(&hub)),
            push: Push::memory(),
            client,
            hub,
            token: token.into(),
            allowed_origins: Vec::new(),
            access_log: false,
            tls: false,
            uploads: default_uploads_dir(),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new().fallback(move |req: Request<Body>| {
        let state = Arc::clone(&state);
        async move { handle(state, req).await }
    })
}

async fn handle(state: Arc<AppState>, req: Request<Body>) -> Response<Body> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let resp = match method {
        Method::GET | Method::HEAD => get(&state, req).await,
        Method::POST => post(&state, req).await,
        _ => json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            &json!({"error": "method_not_allowed"}),
            None,
        ),
    };
    // Off by default: psycommux usually runs in a cmux terminal the browser is
    // watching, and a log line per poll would change that terminal and answer
    // the next poll at once, forever. Errors are always logged.
    if state.access_log || resp.status().as_u16() >= 400 {
        let line = format!("\"{method} {uri}\" {}", resp.status().as_u16())
            .replace(&state.token, "<token>");
        eprintln!("{line}");
    }
    resp
}

fn json_response(
    status: StatusCode,
    payload: &Value,
    headers: Option<&HeaderMap>,
) -> Response<Body> {
    let mut body = serde_json::to_vec(payload).unwrap_or_default();
    let gzip = body.len() > 1024
        && headers
            .and_then(|h| h.get(header::ACCEPT_ENCODING))
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("gzip"));
    if gzip {
        use std::io::Write;
        let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(5));
        z.write_all(&body).unwrap();
        body = z.finish().unwrap();
    }
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store");
    if gzip {
        builder = builder
            .header(header::CONTENT_ENCODING, "gzip")
            .header(header::VARY, "Accept-Encoding");
    }
    builder.body(Body::from(body)).unwrap()
}

fn bytes_response(data: impl Into<Body>, content_type: &str, cache: &str) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache)
        .body(data.into())
        .unwrap()
}

fn page_response(data: &'static str, content_type: &str) -> Response<Body> {
    let mut resp = bytes_response(data, content_type, "no-store");
    resp.headers_mut()
        .insert("content-security-policy", HeaderValue::from_static(CSP));
    resp
}

fn error_status(code: &str) -> StatusCode {
    match code {
        "not_found" => StatusCode::NOT_FOUND,
        "unsupported" | "confirmation_required" => StatusCode::CONFLICT,
        "bad_request" | "too_large" | "invalid_params" => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    }
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == COOKIE_NAME).then(|| value.to_string())
    })
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let mut presented = cookie_token(headers);
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(bearer) = auth.strip_prefix("Bearer ")
    {
        presented = Some(bearer.trim().to_string());
    }
    presented
        .is_some_and(|p| !p.is_empty() && constant_time_eq(p.as_bytes(), state.token.as_bytes()))
}

/// CSRF guard for state-changing requests (on top of SameSite=Lax, which keeps the cookie off cross-site POSTs).
fn same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        // Non-browser clients (curl with a bearer token) send no Origin.
        return true;
    };
    if state.allowed_origins.iter().any(|o| o == origin) {
        return true;
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    netloc(origin) == host
}

fn query_param(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn manifest() -> Value {
    json!({
        "name": "psycommux",
        "short_name": "psycommux",
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#1f1e1d",
        "theme_color": "#1f1e1d",
        "icons": [
            {"src": "/icon-192.png", "sizes": "192x192", "type": "image/png"},
            {"src": "/icon-512.png", "sizes": "512x512", "type": "image/png"},
        ],
    })
}

fn icon_size(path: &str) -> Option<u32> {
    match path {
        "/apple-touch-icon.png" => Some(180),
        "/icon-192.png" => Some(192),
        "/icon-512.png" => Some(512),
        "/favicon.ico" => Some(32),
        _ => None,
    }
}

static ICONS: std::sync::LazyLock<Mutex<HashMap<u32, Vec<u8>>>> =
    std::sync::LazyLock::new(Default::default);

async fn get(state: &Arc<AppState>, req: Request<Body>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let headers = req.headers();
    if path == "/login" {
        return login(state, headers, &query);
    }
    // Fetched by the browser without cookies when adding to the Home Screen; nothing secret.
    if path == "/manifest.webmanifest" {
        return bytes_response(
            manifest().to_string(),
            "application/manifest+json",
            "max-age=86400",
        );
    }
    if let Some(size) = icon_size(&path) {
        let png = ICONS
            .lock()
            .unwrap()
            .entry(size)
            .or_insert_with(|| icon_png(size))
            .clone();
        return bytes_response(png, "image/png", "max-age=86400");
    }
    // The service worker only shows pushed notifications; nothing secret.
    if path == "/sw.js" {
        return page_response(SW_JS, "text/javascript; charset=utf-8");
    }
    if path == "/" || path == "/index.html" {
        if !authorized(state, headers) {
            return login_page(false);
        }
        return page_response(INDEX_HTML, "text/html; charset=utf-8");
    }
    if let Some(name) = path.strip_prefix("/assets/") {
        return match name {
            "app.js" => page_response(APP_JS, "text/javascript; charset=utf-8"),
            "app.css" => page_response(APP_CSS, "text/css; charset=utf-8"),
            _ => json_response(StatusCode::NOT_FOUND, &json!({"error": "not_found"}), None),
        };
    }
    if !authorized(state, headers) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "unauthorized"}),
            None,
        );
    }
    let result = match path.as_str() {
        "/api/tree" => {
            build_tree(&state.client, Some(&state.hub.sidebar), Some(&state.agents)).await
        }
        "/api/notifications" => state
            .client
            .call("notification.list", json!({}))
            .await
            .map(|r| json!({"notifications": or_value(field(&r, "notifications"), json!([]))})),
        "/api/events" => return events(state),
        "/api/secrets" => Ok(json!({"requests": state.secrets.list()})),
        "/api/push" => {
            Ok(json!({"key": state.push.public_key(), "subscriptions": state.push.count()}))
        }
        _ => return json_response(StatusCode::NOT_FOUND, &json!({"error": "not_found"}), None),
    };
    match result {
        Ok(value) => json_response(StatusCode::OK, &value, Some(headers)),
        Err(e) => json_response(
            StatusCode::BAD_GATEWAY,
            &json!({"error": e.code, "message": e.message}),
            None,
        ),
    }
}

async fn post(state: &Arc<AppState>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let headers = &parts.headers;
    if !authorized(state, headers) {
        return json_response(
            StatusCode::UNAUTHORIZED,
            &json!({"error": "unauthorized"}),
            None,
        );
    }
    if !same_origin(state, headers) {
        return json_response(StatusCode::FORBIDDEN, &json!({"error": "bad_origin"}), None);
    }
    if parts.uri.path() == "/api/upload" {
        return match upload(state, headers, body).await {
            Ok(path) => json_response(StatusCode::OK, &json!({"path": path}), Some(headers)),
            Err(e) => error_response(&e),
        };
    }
    let body = match read_json(headers, body).await {
        Ok(body) => body,
        Err(e) => return error_response(&e),
    };
    if parts.uri.path() == "/api/browser/shot" {
        return browser_shot(state, &Body_(&body))
            .await
            .unwrap_or_else(|e| error_response(&e));
    }
    match dispatch_post(state, parts.uri.path(), &body).await {
        Ok(Some(value)) => json_response(StatusCode::OK, &value, Some(headers)),
        Ok(None) => json_response(StatusCode::NOT_FOUND, &json!({"error": "not_found"}), None),
        Err(e) => error_response(&e),
    }
}

/// `~/.psycommux/uploads`.
pub fn default_uploads_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".psycommux")
        .join("uploads")
}

pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// A file name safe to paste into a prompt or a shell: ASCII letters, digits,
/// `.`, `-` and `_`, with an extension from the content type when it has none.
pub fn upload_name(name: &str, content_type: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut clean = String::new();
    for c in base.chars() {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if !(c == '-' && clean.ends_with('-')) {
            clean.push(c);
        }
    }
    let clean = clean.trim_matches(|c| c == '-' || c == '.');
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 => {
            (stem, Some(ext))
        }
        _ => (clean, None),
    };
    let from_type = match content_type.split(';').next().unwrap_or("").trim() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/heic" => Some("heic"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        _ => None,
    };
    let stem: String = stem.chars().take(60).collect();
    let stem = if stem.is_empty() {
        "file".to_string()
    } else {
        stem
    };
    match ext.or(from_type) {
        Some(ext) => format!("{stem}.{}", ext.to_ascii_lowercase()),
        None => stem,
    }
}

/// Keep a file sent from the phone under `state.uploads` (0700 directory,
/// 0600 file, a random prefix so names never collide) and answer its path.
/// Files older than a week are removed on the way.
async fn upload(state: &AppState, headers: &HeaderMap, body: Body) -> Result<String> {
    let header_text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let length: usize = header_text("content-length").parse().unwrap_or(0);
    if length > MAX_UPLOAD_BYTES {
        return Err(CmuxError::new("too_large", "files are limited to 50 MB"));
    }
    let raw_name = header_text("x-filename");
    // encodeURIComponent output: form decoding reads it back exactly.
    let name: String = url::form_urlencoded::parse(raw_name.as_bytes())
        .map(|(k, v)| {
            if v.is_empty() {
                k
            } else {
                format!("{k}={v}").into()
            }
        })
        .collect();
    let name = upload_name(&name, &header_text("content-type"));
    let bytes = axum::body::to_bytes(body, MAX_UPLOAD_BYTES)
        .await
        .map_err(|_| CmuxError::new("too_large", "files are limited to 50 MB"))?;
    if bytes.is_empty() {
        return Err(CmuxError::new("bad_request", "the file is empty"));
    }
    let dir = state.uploads.clone();
    let path = dir.join(format!("{}-{name}", &crate::net::new_token()[..8]));
    let written = path.clone();
    blocking(move || {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > UPLOAD_MAX_AGE);
                if old {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&written)?;
        file.write_all(&bytes)
    })
    .await?;
    Ok(path.to_string_lossy().into_owned())
}

fn error_response(e: &CmuxError) -> Response<Body> {
    json_response(
        error_status(&e.code),
        &json!({"error": e.code, "message": e.message}),
        None,
    )
}

/// The browser panel's screenshot as image bytes (200), or 204 when it still
/// matches the `hash` the viewer has. Either way `x-shot-hash` names the
/// screenshot and `x-page-url` holds the page's URL, percent-encoded.
async fn browser_shot(state: &Arc<AppState>, body: &Body_<'_>) -> Result<Response<Body>> {
    let client = &state.client;
    let surface_id = body.need("surface_id")?;
    if let (Some(w), Some(h)) = (body.int("width"), body.int("height")) {
        state.viewports.hold(&surface_id, w, h).await?;
    }
    let shot = client
        .call("browser.screenshot", json!({"surface_id": surface_id}))
        .await?;
    let png = shot
        .get("png_base64")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let digest = panels::shot_hash(&png);
    let page = client
        .call("browser.url.get", json!({"surface_id": surface_id}))
        .await
        .unwrap_or(Value::Null);
    let url = or_value(
        or_value(field(&page, "url"), field(&shot, "page_url")),
        json!(""),
    );
    let url = url.as_str().unwrap_or("");
    let mut builder = Response::builder()
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-shot-hash", &digest)
        .header("x-page-url", panels::header_safe_url(url));
    if body.opt_str("hash").as_deref() == Some(digest.as_str()) {
        return Ok(builder
            .status(StatusCode::NO_CONTENT)
            .body(Body::empty())
            .unwrap());
    }
    let (bytes, mime) = tokio::task::spawn_blocking(move || panels::shot_image(&png))
        .await
        .map_err(|e| CmuxError::new("error", e.to_string()))?
        .map_err(|e| CmuxError::new("bad_screenshot", e))?;
    builder = builder.header(header::CONTENT_TYPE, mime);
    Ok(builder.body(Body::from(bytes)).unwrap())
}

async fn read_json(headers: &HeaderMap, body: Body) -> Result<Map<String, Value>> {
    let length: usize = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(CmuxError::new("too_large", "request body too large"));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.contains("application/json") {
        return Err(CmuxError::new("bad_request", "expected application/json"));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| CmuxError::new("too_large", "request body too large"))?;
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(CmuxError::new("bad_request", "expected a JSON object")),
        Err(e) => Err(CmuxError::new("bad_request", e.to_string())),
    }
}

/// A request body with typed accessors.
struct Body_<'a>(&'a Map<String, Value>);

impl Body_<'_> {
    fn need(&self, key: &str) -> Result<String> {
        match self.0.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
            _ => Err(CmuxError::new("bad_request", format!("missing {key}"))),
        }
    }

    fn opt_str(&self, key: &str) -> Option<String> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn truthy(&self, key: &str) -> bool {
        self.0.get(key).is_some_and(|v| !is_falsy(v))
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }

    /// A number (not a bool or string), or `default` when absent.
    fn num(&self, key: &str, default: f64) -> Result<f64> {
        match self.0.get(key) {
            None => Ok(default),
            Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(default)),
            Some(_) => Err(CmuxError::new(
                "bad_request",
                format!("{key} must be a number"),
            )),
        }
    }

    /// A byte offset, or None when absent.
    fn offset(&self, key: &str) -> Result<Option<u64>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| {
                CmuxError::new("bad_request", format!("{key} must be a byte offset"))
            }),
        }
    }
}

fn ok_or(value: Value, fallback: Value) -> Value {
    if is_falsy(&value) {
        fallback
    } else {
        value
    }
}

fn io_err(e: std::io::Error) -> CmuxError {
    CmuxError::new("not_found", e.to_string())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CmuxError::new("error", e.to_string()))?
        .map_err(io_err)
}

async fn dispatch_post(
    state: &Arc<AppState>,
    path: &str,
    raw: &Map<String, Value>,
) -> Result<Option<Value>> {
    let body = Body_(raw);
    let client = &state.client;
    let ok = || json!({"ok": true});
    let value = match path {
        "/api/screens" => {
            let Some(map) = raw
                .get("surfaces")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty())
            else {
                return Err(CmuxError::new(
                    "bad_request",
                    "surfaces must be an object of surface_id -> revision",
                ));
            };
            if map.len() > MAX_SCREENS {
                return Err(CmuxError::new(
                    "bad_request",
                    format!("at most {MAX_SCREENS} surfaces per request"),
                ));
            }
            let revisions = revisions_from(map).ok_or_else(|| {
                CmuxError::new("bad_request", "surface ids and revisions must be strings")
            })?;
            let lines = body
                .int("lines")
                .filter(|n| *n != 0)
                .unwrap_or(400)
                .clamp(1, MAX_SCREEN_LINES);
            json!({"screens": client.read_screens(&revisions, lines).await?})
        }
        "/api/send" => {
            let surface_id = body.need("surface_id")?;
            let text = match raw.get("text") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(s)) => s.clone(),
                Some(_) => return Err(CmuxError::new("bad_request", "text must be a string")),
            };
            if !text.is_empty() {
                client
                    .call(
                        "surface.send_text",
                        json!({"surface_id": surface_id, "text": text}),
                    )
                    .await?;
            }
            if body.truthy("enter") {
                client
                    .call(
                        "surface.send_key",
                        json!({"surface_id": surface_id, "key": "enter"}),
                    )
                    .await?;
            }
            ok()
        }
        "/api/key" => {
            client
                .call(
                    "surface.send_key",
                    json!({"surface_id": body.need("surface_id")?, "key": body.need("key")?}),
                )
                .await?;
            ok()
        }
        "/api/peek" => {
            // The visible screen, for prompts the transcript does not show yet,
            // and what cmux says its input area holds: empty, draft, or a
            // dialog such as a menu a slash command opened.
            let surface_id = body.need("surface_id")?;
            let result = client
                .call("surface.read_text", json!({"surface_id": surface_id}))
                .await?;
            let input = client
                .call("surface.input_state", json!({"surface_id": surface_id}))
                .await
                .map(|v| field(&v, "state"))
                .unwrap_or(Value::Null);
            json!({"text": screen_text(&result), "input": input})
        }
        p if p.starts_with("/api/chat/") => {
            return chat(state, &p["/api/chat/".len()..], &body).await
        }
        "/api/browser/release" => {
            state.viewports.release(&body.need("surface_id")?).await;
            ok()
        }
        "/api/browser/act" => {
            // Reader mode keeps the panel laid out at the phone's size, as screenshots do:
            // the page then shows its mobile layout, and an off-screen panel has one at all.
            if let (Some(w), Some(h)) = (body.int("width"), body.int("height")) {
                state
                    .viewports
                    .hold(&body.need("surface_id")?, w, h)
                    .await?;
            }
            browser_act(
                client,
                &body.need("surface_id")?,
                &body.need("action")?,
                &body,
            )
            .await?
        }
        "/api/file" => {
            let surface_id = body.need("surface_id")?;
            let file_path = state
                .files
                .path_for(&surface_id)
                .map(PathBuf::from)
                .filter(|p| p.is_file());
            let Some(file_path) = file_path else {
                return Err(CmuxError::new(
                    "not_found",
                    "cmux has not saved which file this panel shows yet",
                ));
            };
            let known = raw.get("stamp").and_then(Value::as_str).map(str::to_string);
            blocking(move || panels::read_file(&file_path, known.as_deref())).await?
        }
        "/api/session/new" => {
            // A new workspace whose shell starts Claude Code, with the first prompt if given.
            let cwd = body.need("cwd")?;
            let prompt = raw
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let command = if prompt.is_empty() {
                "claude".to_string()
            } else {
                format!("claude {}", shell_quote(prompt))
            };
            let mut params = json!({"cwd": cwd, "initial_input": format!("{command}\n")});
            if let Some(window_id) = body.opt_str("window_id") {
                params["window_id"] = json!(window_id);
            }
            ok_or(client.call("workspace.create", params).await?, json!({}))
        }
        "/api/workspace/select" => {
            // This switches the workspace on the Mac screen too.
            client
                .call(
                    "workspace.select",
                    json!({"workspace_id": body.need("workspace_id")?}),
                )
                .await?;
            ok()
        }
        "/api/workspace/action" => {
            // The Mac sidebar's context menu, from the phone. Closing something
            // with a running process answers confirmation_required (409) unless
            // `force` is set.
            let workspace_id = body.need("workspace_id")?;
            let id = json!(workspace_id);
            let (method, params) = match body.need("action")?.as_str() {
                "rename" => (
                    "workspace.rename",
                    json!({"workspace_id": id, "title": body.need("title")?}),
                ),
                a @ ("pin" | "unpin" | "mark-read" | "mark-unread") => {
                    ("workspace.action", json!({"workspace_id": id, "action": a}))
                }
                "equalize" => ("workspace.equalize_splits", json!({"workspace_id": id})),
                "close" => (
                    "workspace.close",
                    json!({"workspace_id": id, "force": body.truthy("force")}),
                ),
                other => {
                    return Err(CmuxError::new(
                        "bad_request",
                        format!("unknown workspace action {other}"),
                    ))
                }
            };
            ok_or(client.call(method, params).await?, ok())
        }
        "/api/surface/action" => {
            let surface_id = body.need("surface_id")?;
            let id = json!(surface_id);
            let (method, params) = match body.need("action")?.as_str() {
                "rename" => (
                    "tab.action",
                    json!({"tab_id": id, "action": "rename", "title": body.need("title")?}),
                ),
                a @ ("new-terminal-right" | "new-browser-right" | "move-to-new-workspace") => {
                    let mut p = json!({"tab_id": id, "action": a, "focus": false});
                    if let Some(url) = body.opt_str("url") {
                        p["url"] = json!(navigable_url(&url)?);
                    }
                    ("tab.action", p)
                }
                "split-right" | "split-down" => {
                    let direction = if body.need("action")? == "split-right" {
                        "right"
                    } else {
                        "down"
                    };
                    (
                        "surface.split",
                        json!({"surface_id": id, "direction": direction, "focus": false}),
                    )
                }
                "close" => (
                    "surface.close",
                    json!({"surface_id": id, "force": body.truthy("force")}),
                ),
                other => {
                    return Err(CmuxError::new(
                        "bad_request",
                        format!("unknown panel action {other}"),
                    ))
                }
            };
            ok_or(client.call(method, params).await?, ok())
        }
        "/api/workspace/create" => {
            let mut params = json!({});
            for key in ["cwd", "window_id"] {
                if let Some(v) = body.opt_str(key) {
                    params[key] = json!(v);
                }
            }
            ok_or(client.call("workspace.create", params).await?, json!({}))
        }
        "/api/notifications/read" => {
            let params = if body.truthy("all") {
                json!({"all": true})
            } else if let Some(ws) = body.opt_str("workspace_id") {
                // A pane was looked at: mark its notifications read, as focusing it in cmux does.
                let mut params = json!({"workspace_id": ws});
                if let Some(surface) = body.opt_str("surface_id") {
                    params["surface_id"] = json!(surface);
                }
                params
            } else {
                json!({"id": body.need("id")?})
            };
            ok_or(
                client.call("notification.mark_read", params).await?,
                json!({}),
            )
        }
        "/api/notifications/clear_all" => {
            // cmux's "Clear All": dismiss every notification, one by one.
            let list = client.call("notification.list", json!({})).await?;
            let ids: Vec<Value> = items(&list, "notifications")
                .map(|n| field(n, "id"))
                .collect();
            for id in &ids {
                client
                    .call("notification.dismiss", json!({"id": id}))
                    .await?;
            }
            json!({"dismissed": ids.len()})
        }
        "/api/push/subscribe" => {
            let sub = raw.get("subscription").cloned().unwrap_or(Value::Null);
            state
                .push
                .subscribe(&sub)
                .map_err(|e| CmuxError::new("bad_request", e))?;
            json!({"ok": true, "subscriptions": state.push.count()})
        }
        "/api/push/unsubscribe" => {
            json!({"removed": state.push.unsubscribe(&body.need("endpoint")?)})
        }
        "/api/push/test" => {
            let sent = state
                .push
                .send_all(&json!({"title": "psycommux", "body": "Push notifications work.", "tag": "psycommux-test", "url": "/"}))
                .await;
            json!({"sent": sent.sent, "failed": sent.failed})
        }
        "/api/secrets/answer" => {
            let id = body.need("id")?;
            let value = match raw.get("value") {
                Some(Value::String(v)) if !v.is_empty() => v.clone(),
                _ => return Err(CmuxError::new("bad_request", "missing value")),
            };
            if value.len() > MAX_SECRET_BYTES {
                return Err(CmuxError::new("too_large", "secret too large"));
            }
            if !state.secrets.answer(&id, Some(value)) {
                return Err(CmuxError::new(
                    "not_found",
                    "no such request; it may have timed out",
                ));
            }
            ok()
        }
        "/api/secrets/decline" => {
            if !state.secrets.answer(&body.need("id")?, None) {
                return Err(CmuxError::new("not_found", "no such request"));
            }
            ok()
        }
        "/api/notifications/dismiss" => ok_or(
            client
                .call("notification.dismiss", json!({"id": body.need("id")?}))
                .await?,
            json!({}),
        ),
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Only http and https pages; a bare host gets https://. `localhost:8080` is a host, not a scheme.
pub fn navigable_url(url: &str) -> Result<String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(CmuxError::new("bad_request", "url required"));
    }
    let bad = || CmuxError::new("bad_request", "only http and https URLs");
    let scheme_re = regex::Regex::new(r"^([a-zA-Z][\w+.-]*):").unwrap();
    if let Some(caps) = scheme_re.captures(url) {
        let after = &url[caps.get(0).unwrap().end()..];
        let port_like = after.chars().next().is_some_and(|c| c.is_ascii_digit());
        if !port_like && !matches!(caps[1].to_ascii_lowercase().as_str(), "http" | "https") {
            return Err(bad());
        }
    }
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    match url::Url::parse(&url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(url),
        _ => Err(bad()),
    }
}

async fn browser_act(
    client: &CmuxClient,
    surface_id: &str,
    action: &str,
    body: &Body_<'_>,
) -> Result<Value> {
    let target = |extra: Value| {
        let mut params = json!({"surface_id": surface_id});
        if let Value::Object(extra) = extra {
            params.as_object_mut().unwrap().extend(extra);
        }
        params
    };
    let ok = json!({"ok": true});
    let script = match action {
        "tap" => panels::tap_script(body.num("fx", 0.0)?, body.num("fy", 0.0)?),
        "scroll" => panels::scroll_script(
            body.num("fx", 0.5)?,
            body.num("fy", 0.5)?,
            body.num("dx", 0.0)?,
            body.num("dy", 0.0)?,
        ),
        "type" => {
            let text = body
                .opt_str("text")
                .ok_or_else(|| CmuxError::new("bad_request", "text required"))?;
            panels::type_script(&text)
        }
        "key" => {
            let key = body
                .opt_str("key")
                .filter(|k| BROWSER_KEYS.contains(&k.as_str()));
            let key = key.ok_or_else(|| CmuxError::new("bad_request", "unsupported key"))?;
            return Ok(ok_or(
                client
                    .call("browser.press", target(json!({"key": key})))
                    .await?,
                ok,
            ));
        }
        "navigate" => {
            let url = navigable_url(body.0.get("url").and_then(Value::as_str).unwrap_or(""))?;
            return Ok(ok_or(
                client
                    .call("browser.navigate", target(json!({"url": url})))
                    .await?,
                ok,
            ));
        }
        "back" | "forward" | "reload" => {
            return Ok(ok_or(
                client
                    .call(&format!("browser.{action}"), target(json!({})))
                    .await?,
                ok,
            ));
        }
        "read" => {
            let result = client
                .call(
                    "browser.eval",
                    target(json!({"script": panels::READ_SCRIPT})),
                )
                .await?;
            let page = field(&result, "value");
            let digest = panels::shot_hash(&page.to_string());
            if body.opt_str("hash").as_deref() == Some(digest.as_str()) {
                return Ok(json!({"ok": true, "hash": digest, "same": true}));
            }
            return Ok(json!({"ok": true, "hash": digest, "page": page}));
        }
        _ => return Err(CmuxError::new("bad_request", "unknown browser action")),
    };
    let result = client
        .call("browser.eval", target(json!({"script": script})))
        .await?;
    Ok(json!({"ok": true, "value": field(&result, "value")}))
}

async fn chat(state: &Arc<AppState>, action: &str, body: &Body_<'_>) -> Result<Option<Value>> {
    let session_id = body.need("session_id")?;
    let client = &state.client;
    let value = match action {
        "history" => {
            let (record, path) = state.agents.transcript(&session_id).await?;
            let before = body.offset("before")?;
            let mut page =
                blocking(move || agentchat::read_tail(&path, before, 80, agentchat::CHUNK)).await?;
            page["state"] = json!(state_of(&record));
            page
        }
        "poll" => {
            // Long-poll: return once the transcript grows or the agent's state changes.
            let after = body.offset("after")?.unwrap_or(0);
            let known_state = body.0.get("state").cloned().unwrap_or(Value::Null);
            let deadline = Instant::now() + CHAT_POLL;
            loop {
                let (record, path) = state.agents.transcript(&session_id).await?;
                let session_state = state_of(&record);
                let mut update = blocking(move || agentchat::read_since(&path, after)).await?;
                let grew = update["items"].as_array().is_some_and(|a| !a.is_empty());
                let moved = update["end"].as_u64() != Some(after);
                if grew
                    || moved
                    || json!(session_state) != known_state
                    || Instant::now() >= deadline
                {
                    update["state"] = json!(session_state);
                    break update;
                }
                tokio::time::sleep(CHAT_CHECK).await;
            }
        }
        "send" => {
            let text = body.0.get("text").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() {
                return Err(CmuxError::new("bad_request", "text required"));
            }
            match client
                .call(
                    "mobile.chat.send",
                    json!({"session_id": session_id, "text": text}),
                )
                .await
            {
                Ok(v) => ok_or(v, json!({"ok": true})),
                Err(e) if e.code == "not_found" || e.code == "method_not_found" => {
                    // cmux lost the session's terminal binding; paste into the surface it last reported.
                    let record = state.agents.get(&session_id).await?;
                    let surface = field(&record, "surface_id");
                    for key in ["ctrl+a", "ctrl+k", "ctrl+u"] {
                        client
                            .call(
                                "surface.send_key",
                                json!({"surface_id": surface, "key": key}),
                            )
                            .await?;
                    }
                    let params = json!({"surface_id": surface, "workspace_id": field(&record, "workspace_id"), "text": text});
                    ok_or(
                        client.call("terminal.paste", params).await?,
                        json!({"ok": true}),
                    )
                }
                Err(e) => return Err(e),
            }
        }
        "interrupt" => {
            let hard = body.truthy("hard");
            match client
                .call(
                    "mobile.chat.interrupt",
                    json!({"session_id": session_id, "hard": hard}),
                )
                .await
            {
                Ok(v) => ok_or(v, json!({"ok": true})),
                Err(e) if e.code == "not_found" || e.code == "method_not_found" => {
                    let record = state.agents.get(&session_id).await?;
                    let key = if hard { "ctrl+c" } else { "escape" };
                    client
                        .call(
                            "surface.send_key",
                            json!({"surface_id": field(&record, "surface_id"), "key": key}),
                        )
                        .await?;
                    json!({"interrupted": true, "hard": hard})
                }
                Err(e) => return Err(e),
            }
        }
        "resume" => {
            // Start an ended session again in its terminal, from the folder it ran in.
            let (record, path) = state.agents.transcript(&session_id).await?;
            if state_of(&record) != "ended" {
                return Err(CmuxError::new(
                    "bad_request",
                    "this session is still running",
                ));
            }
            if !session_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-')
            {
                return Err(CmuxError::new("bad_request", "not a Claude session id"));
            }
            let surface = field(&record, "surface_id");
            let cwd = blocking(move || Ok(agentchat::session_cwd(&path))).await?;
            let command = match cwd {
                Some(dir) => format!("cd {} && claude --resume {session_id}", shell_quote(&dir)),
                None => format!("claude --resume {session_id}"),
            };
            client
                .call(
                    "surface.send_text",
                    json!({"surface_id": surface, "text": command}),
                )
                .await?;
            client
                .call(
                    "surface.send_key",
                    json!({"surface_id": surface, "key": "enter"}),
                )
                .await?;
            json!({"ok": true, "surface_id": surface})
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn login(state: &AppState, headers: &HeaderMap, query: &str) -> Response<Body> {
    let presented = query_param(query, "token").unwrap_or_default();
    if presented.is_empty() || !constant_time_eq(presented.as_bytes(), state.token.as_bytes()) {
        return login_page(true);
    }
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        == Some("https");
    let secure = if state.tls || forwarded_https {
        "; Secure"
    } else {
        ""
    };
    // Not an HTTP redirect: a link opened from another app (the iPhone camera,
    // Messages) is a cross-site navigation and a redirect stays part of it. A
    // navigation started by this page is same-site, so the cookie goes along.
    // The cookie is Lax rather than Strict because an iOS Home Screen app
    // launched cold opens "/" as if from outside the site, and Strict would
    // send it to the login page each time.
    let html = concat!(
        "<!doctype html><meta http-equiv=\"refresh\" content=\"0;url=/\">",
        "<title>psycommux</title>",
        "<script>location.replace(\"/\")</script>",
        "<body style='font-family:system-ui;background:#1f1e1d;color:#ddd;padding:2em'>",
        "<a href=/ style='color:#d97757'>Continue</a>",
    );
    let cookie = format!(
        "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000{secure}",
        state.token
    );
    Response::builder()
        .header(header::SET_COOKIE, cookie)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("referrer-policy", "no-referrer")
        .body(Body::from(html))
        .unwrap()
}

fn login_page(error: bool) -> Response<Body> {
    let msg = if error {
        "<p style='color:#e66'>Wrong token.</p>"
    } else {
        ""
    };
    let html = format!(
        "<!doctype html><meta name=viewport content='width=device-width,initial-scale=1'>\
         <title>psycommux</title><body style='font-family:system-ui;background:#1f1e1d;color:#ddd;padding:2em'>\
         <h2>psycommux</h2>{msg}\
         <form action=/login method=get><input name=token type=password placeholder='access token' \
         autofocus style='font-size:1.1em;padding:.4em'> <button>Sign in</button></form>"
    );
    Response::builder()
        .status(if error {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap()
}

fn sse(message: &Value) -> Result<bytes::Bytes> {
    Ok(bytes::Bytes::from(format!("data: {message}\n\n")))
}

fn events(state: &Arc<AppState>) -> Response<Body> {
    let rx = state.hub.subscribe();
    let first = json!({"type": "status", "connected": state.hub.connected(), "resync": false});
    let stream = stream::unfold((rx, Some(first)), |(mut rx, first)| async move {
        if let Some(first) = first {
            return Some((sse(&first), (rx, None)));
        }
        loop {
            match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await {
                Err(_) => {
                    return Some((
                        Ok(bytes::Bytes::from_static(b": keepalive\n\n")),
                        (rx, None),
                    ))
                }
                Ok(Ok(message)) => return Some((sse(&message), (rx, None))),
                // A stalled tab lost events; it refreshes on reconnect.
                Ok(Err(RecvError::Lagged(_))) => continue,
                Ok(Err(RecvError::Closed)) => return None,
            }
        }
    });
    let stream = futures_util::StreamExt::map(stream, |r| {
        r.map_err(|e| std::io::Error::other(e.to_string()))
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap()
}
