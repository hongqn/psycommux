//! cmux events.stream -> browser Server-Sent Events, plus the sidebar rows
//! (status pills, newest log line, progress) cmux only exposes through v1.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Semaphore};

use crate::cmux::{decode_frame, value_text, CmuxClient, CmuxError};

/// Surface events that change the tree; input/key echoes are skipped.
const SKIPPED_EVENTS: [&str; 3] = ["surface.input_sent", "surface.key_sent", "surface.action"];
const WORKING_STATES: [&str; 2] = ["running", "subagents"];
const SIDEBAR_FETCH_WORKERS: usize = 8;

/// Parse the v1 `sidebar_state` listing into what a sidebar row shows.
///
/// Status pills keep cmux's order, highest priority first. Only the newest
/// log entry is kept, as the sidebar row shows only that one.
pub fn parse_sidebar_state<S: AsRef<str>>(lines: &[S]) -> Value {
    let attr_re = Regex::new(r" (icon|color|url|priority|format|work)=(\S+)$").unwrap();
    let log_re = Regex::new(r"^\[(\w+)\] (.*)$").unwrap();
    let mut statuses: Vec<(i64, Value)> = Vec::new();
    let mut log = Value::Null;
    let mut progress = Value::Null;
    let mut git_dirty = false;
    let mut section = "";
    for raw in lines {
        let raw = raw.as_ref();
        if let Some(item) = raw.strip_prefix("  ") {
            if section == "status" {
                let mut item = item.to_string();
                let mut attrs: HashMap<String, String> = HashMap::new();
                while let Some(caps) = attr_re.captures(&item) {
                    let start = caps.get(0).unwrap().start();
                    attrs.insert(caps[1].to_string(), caps[2].to_string());
                    item.truncate(start);
                }
                let (key, value) = item.split_once('=').unwrap_or((item.as_str(), ""));
                let priority = attrs
                    .get("priority")
                    .and_then(|p| p.parse::<i64>().ok())
                    .unwrap_or(0);
                let attr = |name: &str| {
                    attrs
                        .get(name)
                        .cloned()
                        .map(Value::String)
                        .unwrap_or(Value::Null)
                };
                statuses.push((
                    priority,
                    json!({
                        "key": key, "value": value,
                        "icon": attr("icon"), "color": attr("color"), "url": attr("url"),
                        "priority": priority,
                        "format": attrs.get("format").cloned().unwrap_or_else(|| "plain".into()),
                        "work": attr("work"),
                    }),
                ));
            } else if section == "log" {
                if let Some(caps) = log_re.captures(item) {
                    log = json!({"level": &caps[1], "message": &caps[2]});
                }
            }
            continue;
        }
        let (key, value) = raw.split_once('=').unwrap_or((raw, ""));
        section = match key {
            "status_count" => "status",
            "log_count" => "log",
            _ => "",
        };
        if key == "git_branch" {
            git_dirty = value.ends_with(" dirty");
        } else if key == "progress" && value != "none" {
            let (number, label) = value.split_once(' ').unwrap_or((value, ""));
            progress = match number.parse::<f64>() {
                Ok(n) => {
                    json!({"value": n, "label": if label.is_empty() { Value::Null } else { json!(label) }})
                }
                Err(_) => Value::Null,
            };
        }
    }
    statuses.sort_by_key(|(priority, _)| -priority); // stable: ties keep cmux's order
    let working = statuses.iter().any(|(_, s)| {
        s.get("work")
            .and_then(Value::as_str)
            .is_some_and(|w| WORKING_STATES.contains(&w))
    });
    json!({
        "statuses": statuses.into_iter().map(|(_, s)| s).collect::<Vec<_>>(),
        "log": log,
        "progress": progress,
        "working": working,
        "git_dirty": git_dirty,
    })
}

/// Per-workspace sidebar rows: all workspaces once, then one per sidebar event.
pub struct SidebarCache {
    client: Arc<CmuxClient>,
    entries: Mutex<HashMap<String, Value>>,
    pending: Mutex<HashSet<String>>,
    workers: Semaphore,
    events: broadcast::Sender<Value>,
}

impl SidebarCache {
    fn new(client: Arc<CmuxClient>, events: broadcast::Sender<Value>) -> Self {
        Self {
            client,
            entries: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            workers: Semaphore::new(SIDEBAR_FETCH_WORKERS),
            events,
        }
    }

    pub async fn fetch(&self, workspace_id: &str) -> Option<Value> {
        let entry = {
            let _permit = self.workers.acquire().await.ok()?;
            match self
                .client
                .v1_lines(&format!("sidebar_state --tab={workspace_id}"))
                .await
            {
                Ok(lines) => Some(parse_sidebar_state(&lines)),
                Err(_) => None,
            }
        };
        self.pending.lock().unwrap().remove(workspace_id);
        if let Some(entry) = &entry {
            self.entries
                .lock()
                .unwrap()
                .insert(workspace_id.to_string(), entry.clone());
            let _ = self
                .events
                .send(json!({"type": "sidebar", "workspace_id": workspace_id, "sidebar": entry}));
        }
        entry
    }

    /// Fetch in the background the workspaces not cached or queued yet.
    pub fn ensure(self: &Arc<Self>, workspace_ids: &[String]) {
        let missing: Vec<String> = {
            let entries = self.entries.lock().unwrap();
            let mut pending = self.pending.lock().unwrap();
            let missing: Vec<String> = workspace_ids
                .iter()
                .filter(|w| !entries.contains_key(*w) && !pending.contains(*w))
                .cloned()
                .collect();
            pending.extend(missing.iter().cloned());
            missing
        };
        for workspace_id in missing {
            self.update_later(workspace_id);
        }
    }

    pub fn update_later(self: &Arc<Self>, workspace_id: String) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.fetch(&workspace_id).await;
        });
    }

    pub fn get(&self, workspace_id: &str) -> Option<Value> {
        self.entries.lock().unwrap().get(workspace_id).cloned()
    }
}

/// Keeps one events.stream subscription and fans frames out to browsers.
pub struct EventHub {
    client: Arc<CmuxClient>,
    pub sidebar: Arc<SidebarCache>,
    sender: broadcast::Sender<Value>,
    connected: AtomicBool,
    last_seq: Mutex<Option<i64>>,
    boot_id: Mutex<Option<String>>,
}

impl EventHub {
    pub fn new(client: Arc<CmuxClient>) -> Arc<Self> {
        // A stalled browser tab lags behind and loses events; it refreshes on reconnect.
        let (sender, _) = broadcast::channel(256);
        Arc::new(Self {
            sidebar: Arc::new(SidebarCache::new(Arc::clone(&client), sender.clone())),
            client,
            sender,
            connected: AtomicBool::new(false),
            last_seq: Mutex::new(None),
            boot_id: Mutex::new(None),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.sender.subscribe()
    }

    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// The newest event sequence number seen, to resume from after a reconnect.
    pub fn last_seq(&self) -> Option<i64> {
        *self.last_seq.lock().unwrap()
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn publish(&self, message: Value) {
        let _ = self.sender.send(message);
    }

    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                match this.consume().await {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(e) => {
                        if this.connected.swap(false, Ordering::Relaxed) {
                            this.publish(json!({"type": "status", "connected": false, "error": e.to_string()}));
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(15));
                    }
                }
            }
        });
    }

    async fn consume(&self) -> Result<(), CmuxError> {
        let after = *self.last_seq.lock().unwrap();
        let mut conn = self.client.open_event_stream(after).await?;
        loop {
            let line = conn.read_line(Duration::from_secs(60)).await.map_err(|e| {
                if e.code == "timeout" {
                    CmuxError::new("timeout", "no heartbeat from cmux")
                } else {
                    e
                }
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let frame = Value::Object(decode_frame(&line)?);
            if frame.get("ok") == Some(&Value::Bool(false)) {
                let err = frame.get("error").cloned().unwrap_or(json!({}));
                return Err(error_from(&err, "error"));
            }
            match frame.get("type").and_then(Value::as_str) {
                Some("ack") => {
                    let mut gap = frame
                        .pointer("/resume/gap")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let boot = frame
                        .get("boot_id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    {
                        let mut known = self.boot_id.lock().unwrap();
                        if known.is_some() && *known != boot {
                            gap = true;
                        }
                        *known = boot;
                    }
                    self.connected.store(true, Ordering::Relaxed);
                    // A gap means events were missed: browsers reload snapshots.
                    self.publish(json!({"type": "status", "connected": true, "resync": gap}));
                }
                Some("event") => self.handle_event(&frame).await,
                Some("error") => {
                    let err = frame.get("error").cloned().unwrap_or(json!({}));
                    return Err(error_from(&err, "stream_error"));
                }
                _ => {}
            }
        }
    }

    async fn handle_event(&self, frame: &Value) {
        if let Some(seq) = frame.get("seq").and_then(Value::as_i64) {
            *self.last_seq.lock().unwrap() = Some(seq);
        }
        let name = frame.get("name").and_then(Value::as_str).unwrap_or("");
        if SKIPPED_EVENTS.contains(&name) {
            return;
        }
        if name.starts_with("sidebar.") || name == "program_status.changed" {
            // The row's pills changed: re-read that workspace, which then tells browsers.
            if let Some(ws) = frame
                .get("workspace_id")
                .map(value_text)
                .filter(|s| !s.is_empty())
            {
                self.sidebar.update_later(ws);
            }
            return;
        }
        let field = |key: &str| frame.get(key).cloned().unwrap_or(Value::Null);
        let mut message = json!({
            "type": "event",
            "name": field("name"),
            "category": field("category"),
            "workspace_id": field("workspace_id"),
            "surface_id": field("surface_id"),
            "occurred_at": field("occurred_at"),
        });
        if name == "notification.created" {
            // The event stream redacts title/body; fetch the full record.
            let id = frame
                .pointer("/payload/notification_id")
                .map(value_text)
                .unwrap_or_default();
            message["notification"] = self.lookup_notification(&id).await.unwrap_or(Value::Null);
        }
        self.publish(message);
    }

    async fn lookup_notification(&self, id: &str) -> Option<Value> {
        if id.is_empty() {
            return None;
        }
        let result = self
            .client
            .call("notification.list", json!({}))
            .await
            .ok()?;
        result
            .get("notifications")?
            .as_array()?
            .iter()
            .find(|item| {
                item.get("id")
                    .map(value_text)
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(id)
            })
            .cloned()
    }
}

fn error_from(err: &Value, default_code: &str) -> CmuxError {
    let code = err
        .get("code")
        .map(value_text)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_code.into());
    CmuxError::new(code, err.get("message").map(value_text).unwrap_or_default())
}
