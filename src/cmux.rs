//! Client for the cmux v2 JSON-lines control socket.

use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout, Instant};

/// Categories the event stream subscribes to. "sidebar" carries status pills,
/// logs and progress; "terminal" carries program_status.changed.
pub const EVENT_CATEGORIES: [&str; 5] = [
    "notification",
    "workspace",
    "surface",
    "sidebar",
    "terminal",
];
pub const WAIT_TIMEOUT_MS: u64 = 25_000;
/// Poll interval when cmux lacks surface.wait_text_change (0.65.x and older).
const FALLBACK_POLL: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmuxError {
    pub code: String,
    pub message: String,
}

impl CmuxError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CmuxError {}

pub type Result<T> = std::result::Result<T, CmuxError>;

/// Parse one socket reply. cmux refuses with a plain-text ERROR line.
pub fn decode_frame(line: &str) -> Result<Map<String, Value>> {
    if line.starts_with("ERROR") {
        let text = line.trim();
        if text.contains("Access denied") {
            return Err(CmuxError::new(
                "access_denied",
                format!(
                    "{text}. Start psycommux in the foreground of a cmux terminal (not nohup/launchd), \
                     or set the socket to password mode and export CMUX_SOCKET_PASSWORD."
                ),
            ));
        }
        return Err(CmuxError::new("socket_error", text));
    }
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(CmuxError::new("bad_response", "non-object response")),
        Err(_) => {
            let head: String = line.chars().take(200).collect();
            Err(CmuxError::new(
                "bad_response",
                format!("unexpected reply: {head:?}"),
            ))
        }
    }
}

/// surface.read_text answers with text, base64, or both.
pub fn screen_text(result: &Value) -> String {
    if let Some(text) = result.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    result
        .get("base64")
        .and_then(Value::as_str)
        .and_then(|raw| base64::engine::general_purpose::STANDARD.decode(raw).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// The first 16 hex digits of the SHA-1 of `data`.
pub fn short_hash(data: &[u8]) -> String {
    let mut digest = sha1_smol::Sha1::from(data).hexdigest();
    digest.truncate(16);
    digest
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn int_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn truthy(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Shrink a cmux.render-grid.v1 frame (from terminal.replay) for the browser.
///
/// Styles become CSS strings keyed by id; spans become `[column, cells, text,
/// style]` rows, scrollback first, then the viewport.
pub fn compact_grid(grid: &Value) -> Value {
    let term_fg = str_of(grid, "terminal_foreground")
        .filter(|s| !s.is_empty())
        .unwrap_or("#FFFFFF");
    let term_bg = str_of(grid, "terminal_background")
        .filter(|s| !s.is_empty())
        .unwrap_or("#000000");
    let mut styles = Map::new();
    for st in grid
        .get("styles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let source = |key: &str| str_of(st, key).unwrap_or("default").to_string();
        let mut fg = if source("foreground_source") != "default" {
            str_of(st, "foreground")
        } else {
            None
        };
        let mut bg = if source("background_source") != "default" {
            str_of(st, "background")
        } else {
            None
        };
        if truthy(st, "inverse") {
            let (f, b) = (bg.unwrap_or(term_bg), fg.unwrap_or(term_fg));
            fg = Some(f);
            bg = Some(b);
        }
        let mut css: Vec<String> = Vec::new();
        if let Some(fg) = fg.filter(|s| !s.is_empty()) {
            css.push(format!("color:{fg}"));
        }
        if let Some(bg) = bg.filter(|s| !s.is_empty()) {
            css.push(format!("background:{bg}"));
        }
        if truthy(st, "bold") {
            css.push("font-weight:bold".into());
        }
        if truthy(st, "italic") {
            css.push("font-style:italic".into());
        }
        if truthy(st, "faint") {
            css.push("opacity:.6".into());
        }
        let deco: Vec<&str> = [
            ("underline", "underline"),
            ("strikethrough", "line-through"),
            ("overline", "overline"),
        ]
        .iter()
        .filter(|(key, _)| truthy(st, key))
        .map(|(_, name)| *name)
        .collect();
        if !deco.is_empty() {
            css.push(format!("text-decoration:{}", deco.join(" ")));
        }
        if truthy(st, "invisible") {
            css.push("visibility:hidden".into());
        }
        let id = match st.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "None".into(),
        };
        styles.insert(id, Value::String(css.join(";")));
    }

    let scrollback = int_of(grid, "scrollback_rows").max(0) as usize;
    let rows = int_of(grid, "rows").max(0) as usize;
    let mut lines: Vec<Vec<Value>> = vec![Vec::new(); scrollback + rows];
    for (offset, key) in [(0usize, "scrollback_spans"), (scrollback, "row_spans")] {
        for span in grid
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let row = span.get("row").and_then(Value::as_i64).unwrap_or(-1);
            let index = offset as i64 + row;
            if index >= 0 && (index as usize) < lines.len() {
                lines[index as usize].push(json!([
                    span.get("column").cloned().unwrap_or(json!(0)),
                    span.get("cell_width").cloned().unwrap_or(json!(0)),
                    span.get("text").cloned().unwrap_or(json!("")),
                    span.get("style_id").cloned().unwrap_or(json!(0)),
                ]));
            }
        }
    }
    for line in &mut lines {
        line.sort_by_key(|item| item[0].as_i64().unwrap_or(0));
    }

    let cursor = grid.get("cursor").cloned().unwrap_or(json!({}));
    json!({
        "columns": grid.get("columns").cloned().unwrap_or(Value::Null),
        "rows": rows,
        "foreground": term_fg,
        "background": term_bg,
        "styles": styles,
        "lines": lines,
        "cursor": {
            "line": scrollback as i64 + int_of(&cursor, "row"),
            "column": int_of(&cursor, "column"),
            "visible": truthy(&cursor, "visible"),
        },
    })
}

/// Mirror the cmux CLI: CMUX_SOCKET_PATH, then the stable socket.
pub fn default_socket_path() -> String {
    for var in ["CMUX_SOCKET_PATH", "CMUX_SOCKET"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/state/cmux/cmux.sock"),
        format!("{home}/Library/Application Support/cmux/cmux.sock"),
        "/tmp/cmux.sock".to_string(),
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return path.clone();
        }
    }
    candidates[0].clone()
}

/// One connection to the cmux socket, reading newline-delimited frames.
pub struct LineSocket {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl LineSocket {
    pub async fn connect(path: &str, password: Option<&str>) -> Result<Self> {
        let stream = UnixStream::connect(path).await.map_err(|e| {
            CmuxError::new(
                "socket_unavailable",
                format!("cannot connect to {path}: {e}"),
            )
        })?;
        let (read, write) = stream.into_split();
        let mut conn = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        if let Some(password) = password.filter(|p| !p.is_empty()) {
            conn.write_line(&format!("auth {password}")).await?;
            let reply = conn.read_line(Duration::from_secs(5)).await?;
            if reply.starts_with("ERROR") && !reply.contains("Unknown command 'auth'") {
                return Err(CmuxError::new("auth_failed", reply.trim()));
            }
        }
        Ok(conn)
    }

    pub async fn write_line(&mut self, line: &str) -> Result<()> {
        let mut data = line.as_bytes().to_vec();
        data.push(b'\n');
        self.writer
            .write_all(&data)
            .await
            .map_err(|e| CmuxError::new("socket_closed", e.to_string()))
    }

    /// The next line, or a `timeout` error after `wait` without one.
    pub async fn read_line(&mut self, wait: Duration) -> Result<String> {
        let mut buf = Vec::new();
        match timeout(wait, self.reader.read_until(b'\n', &mut buf)).await {
            Err(_) => Err(CmuxError::new("timeout", "no reply from cmux")),
            Ok(Err(e)) => Err(CmuxError::new("socket_closed", e.to_string())),
            Ok(Ok(0)) => Err(CmuxError::new(
                "socket_closed",
                "cmux closed the connection",
            )),
            Ok(Ok(_)) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                } else {
                    return Err(CmuxError::new(
                        "socket_closed",
                        "cmux closed the connection",
                    ));
                }
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }
        }
    }
}

const WAIT_UNKNOWN: u8 = 0;
const WAIT_YES: u8 = 1;
const WAIT_NO: u8 = 2;

pub struct CmuxClient {
    pub socket_path: String,
    pub password: Option<String>,
    next_id: AtomicU64,
    /// Whether cmux has surface.wait_text_change; unknown until the first screen read.
    has_wait_text_change: AtomicU8,
    /// How long a screen read waits for the text to change.
    pub wait_timeout: Duration,
    /// Poll interval when cmux lacks surface.wait_text_change (0.65.x and older).
    pub fallback_poll: Duration,
}

impl CmuxClient {
    pub fn new(socket_path: impl Into<String>, password: Option<String>) -> Self {
        Self {
            socket_path: socket_path.into(),
            password,
            next_id: AtomicU64::new(0),
            has_wait_text_change: AtomicU8::new(WAIT_UNKNOWN),
            wait_timeout: Duration::from_millis(WAIT_TIMEOUT_MS),
            fallback_poll: FALLBACK_POLL,
        }
    }

    fn new_id(&self) -> String {
        format!("web-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub async fn connect(&self) -> Result<LineSocket> {
        LineSocket::connect(&self.socket_path, self.password.as_deref()).await
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(method, params, DEFAULT_TIMEOUT).await
    }

    /// One request per connection: simple, and safe across tasks.
    pub async fn call_timeout(&self, method: &str, params: Value, wait: Duration) -> Result<Value> {
        let mut conn = self.connect().await?;
        let params = if params.is_null() { json!({}) } else { params };
        let request = json!({"id": self.new_id(), "method": method, "params": params});
        conn.write_line(&request.to_string()).await?;
        let line = conn.read_line(wait).await.map_err(|e| {
            if e.code == "timeout" {
                CmuxError::new("timeout", format!("{method} timed out"))
            } else {
                e
            }
        })?;
        let resp = decode_frame(&line)?;
        if resp.get("ok") == Some(&Value::Bool(true)) {
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
        }
        let err = resp.get("error").cloned().unwrap_or(json!({}));
        let code = err
            .get("code")
            .map(value_text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "error".into());
        let message = err
            .get("message")
            .map(value_text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown error".into());
        Err(CmuxError::new(code, message))
    }

    /// Terminal text plus a revision; with `after_revision`, wait for a change.
    ///
    /// Uses surface.wait_text_change when cmux has it, else polls
    /// surface.read_text and derives the revision from the text.
    pub async fn read_screen(
        &self,
        surface_id: &str,
        lines: i64,
        after_revision: &str,
    ) -> Result<Value> {
        let params = json!({"surface_id": surface_id, "scrollback": true, "lines": lines});
        if self.has_wait_text_change.load(Ordering::Relaxed) != WAIT_NO {
            let mut native = params.clone();
            if !after_revision.is_empty() {
                native["after_revision"] = json!(after_revision);
                native["timeout_ms"] = json!(self.wait_timeout.as_millis() as u64);
            }
            let wait = self.wait_timeout + Duration::from_secs(10);
            match self
                .call_timeout("surface.wait_text_change", native, wait)
                .await
            {
                Ok(result) => {
                    self.has_wait_text_change.store(WAIT_YES, Ordering::Relaxed);
                    return Ok(json!({
                        "text": screen_text(&result),
                        "revision": result.get("revision").cloned().unwrap_or(Value::Null),
                        "changed": result.get("changed").cloned().unwrap_or(Value::Bool(true)),
                    }));
                }
                Err(e) if e.code == "method_not_found" => {
                    self.has_wait_text_change.store(WAIT_NO, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        let deadline = Instant::now() + self.wait_timeout;
        loop {
            let text = screen_text(&self.call("surface.read_text", params.clone()).await?);
            let revision = short_hash(text.as_bytes());
            if revision != after_revision {
                return Ok(json!({"text": text, "revision": revision, "changed": true}));
            }
            if Instant::now() >= deadline {
                return Ok(json!({"text": text, "revision": revision, "changed": false}));
            }
            sleep(self.fallback_poll).await;
        }
    }

    /// Wait until any of several terminals changes; attach a colored grid.
    ///
    /// One surface uses read_screen (native wait when cmux has it). Several
    /// are polled together, so one request covers every visible pane.
    pub async fn read_screens(
        &self,
        revisions: &[(String, String)],
        lines: i64,
    ) -> Result<Map<String, Value>> {
        let mut results = if revisions.len() == 1 {
            let (surface_id, after) = &revisions[0];
            let mut map = Map::new();
            map.insert(
                surface_id.clone(),
                self.read_screen(surface_id, lines, after).await?,
            );
            map
        } else {
            self.poll_screens(revisions, lines).await
        };
        for (surface_id, result) in results.iter_mut() {
            if truthy(result, "changed") && result.get("error").is_none() {
                if let Some(grid) = self.read_grid(surface_id).await {
                    result["grid"] = grid;
                }
            }
        }
        Ok(results)
    }

    async fn poll_screens(&self, revisions: &[(String, String)], lines: i64) -> Map<String, Value> {
        let deadline = Instant::now() + self.wait_timeout;
        loop {
            let mut results = Map::new();
            for (surface_id, after) in revisions {
                let params = json!({"surface_id": surface_id, "scrollback": true, "lines": lines});
                match self.call("surface.read_text", params).await {
                    Ok(result) => {
                        let text = screen_text(&result);
                        let revision = short_hash(text.as_bytes());
                        let changed = &revision != after;
                        results.insert(
                            surface_id.clone(),
                            json!({"text": text, "revision": revision, "changed": changed}),
                        );
                    }
                    Err(e) => {
                        results.insert(
                            surface_id.clone(),
                            json!({"error": e.code, "message": e.message, "changed": true}),
                        );
                    }
                }
            }
            if results.values().any(|r| truthy(r, "changed")) || Instant::now() >= deadline {
                return results;
            }
            sleep(self.fallback_poll).await;
        }
    }

    /// The colored screen via terminal.replay, or None when unavailable.
    pub async fn read_grid(&self, surface_id: &str) -> Option<Value> {
        let result = self
            .call("terminal.replay", json!({"surface_id": surface_id}))
            .await
            .ok()?;
        let grid = result.get("render_grid")?;
        grid.is_object().then(|| compact_grid(grid))
    }

    pub async fn open_event_stream(&self, after_seq: Option<i64>) -> Result<LineSocket> {
        let mut conn = self.connect().await?;
        let mut params = json!({"categories": EVENT_CATEGORIES, "include_heartbeats": true});
        if let Some(seq) = after_seq {
            params["after_seq"] = json!(seq);
        }
        let request = json!({"id": self.new_id(), "method": "events.stream", "params": params});
        conn.write_line(&request.to_string()).await?;
        Ok(conn)
    }

    /// Send one v1 text command and read its reply to the end.
    ///
    /// A v1 reply has no terminator; stop after a short idle gap, or once the
    /// log lines `sidebar_state` announces (at most the 5 newest) have arrived.
    pub async fn v1_lines(&self, command: &str) -> Result<Vec<String>> {
        let mut conn = self.connect().await?;
        conn.write_line(command).await?;
        let first = conn.read_line(Duration::from_secs(5)).await?;
        if first.starts_with("ERROR") {
            return Err(CmuxError::new("v1_error", first));
        }
        let mut lines = vec![first];
        let mut remaining: Option<i64> = None;
        while remaining.is_none_or(|n| n > 0) {
            let Ok(line) = conn.read_line(Duration::from_millis(120)).await else {
                break;
            };
            if let Some(n) = remaining.as_mut() {
                *n -= 1;
            } else if let Some(count) = line.strip_prefix("log_count=") {
                remaining = Some(count.parse::<i64>().unwrap_or(0).min(5));
            }
            lines.push(line);
        }
        Ok(lines)
    }
}

/// A JSON value as text, without quotes for strings (Python's `str()`).
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Helper used by several modules: `{"k": v, ...}` lookups as `&str`.
pub fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    str_of(v, key)
}

/// `revisions` from a JSON object, in insertion order of the parsed map.
pub fn revisions_from(map: &Map<String, Value>) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (k, v) in map {
        let v = v.as_str()?;
        if k.is_empty() {
            return None;
        }
        out.push((k.clone(), v.to_string()));
    }
    Some(out)
}
