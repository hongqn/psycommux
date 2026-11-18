//! A fake cmux socket and helpers to drive the router without a network.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Response};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tower::ServiceExt;

use psycommux::cmux::CmuxClient;
use psycommux::events::EventHub;
use psycommux::web::{router, AppState, COOKIE_NAME};

pub const WS_ID: &str = "11111111-1111-1111-1111-111111111111";
pub const SURF_ID: &str = "22222222-2222-2222-2222-222222222222";
pub const WIN_ID: &str = "33333333-3333-3333-3333-333333333333";
pub const NOTIF_ID: &str = "44444444-4444-4444-4444-444444444444";
pub const SURF2_ID: &str = "55555555-5555-5555-5555-555555555555";
pub const BROWSER_ID: &str = "66666666-6666-6666-6666-666666666666";
pub const PANE1_ID: &str = "77777777-7777-7777-7777-777777777777";
pub const PANE2_ID: &str = "88888888-8888-8888-8888-888888888888";
pub const NOTIF2_ID: &str = "99999999-9999-9999-9999-999999999999";
pub const TOKEN: &str = "test-token";
pub const PASSWORD: &str = "sock-pass";

pub fn home() -> String {
    std::env::var("HOME").unwrap()
}

/// The v1 `sidebar_state` text cmux 0.65 prints.
pub fn sidebar_state() -> Vec<String> {
    let home = home();
    [
        format!("tab={WS_ID}"),
        "color=#C0392B".into(),
        format!("cwd={home}/src/api"),
        "focused_cwd=unknown".into(),
        "focused_panel=unknown".into(),
        "git_branch=main dirty".into(),
        "pr=none".into(),
        "pr_label=none".into(),
        "ports=8787".into(),
        "progress=0.40 Indexing".into(),
        "status_count=3".into(),
        "  claude_code=Running icon=bolt.fill color=#4C8DFF work=running".into(),
        "  program_status=claude-code · Working icon=bolt.fill priority=2".into(),
        "  docs=[spec](https://example.com/spec) format=markdown url=https://example.com/spec"
            .into(),
        "meta_block_count=0".into(),
        "log_count=7".into(),
        "  [info] first".into(),
        "  [info] second".into(),
        "  [warning] disk almost full".into(),
        "  [info] third".into(),
        "  [success] 合并。 latest wins".into(),
    ]
    .to_vec()
}

pub fn render_grid() -> Value {
    json!({
        "format": "cmux.render-grid.v1",
        "columns": 20, "rows": 2, "scrollback_rows": 1,
        "terminal_foreground": "#FFFFFF", "terminal_background": "#1E1E1E",
        "cursor": {"row": 1, "column": 4, "visible": true, "style": "block"},
        "styles": [
            {"id": 0, "foreground": "#FFFFFF", "foreground_source": "default",
             "background": "#1E1E1E", "background_source": "default"},
            {"id": 1, "foreground": "#CC372E", "foreground_source": "palette",
             "background": "#1E1E1E", "background_source": "default", "bold": true},
            {"id": 2, "foreground": "#FFFFFF", "foreground_source": "default",
             "background": "#1E1E1E", "background_source": "default", "inverse": true},
        ],
        "scrollback_spans": [{"row": 0, "column": 0, "cell_width": 3, "text": "old", "style_id": 0}],
        "row_spans": [
            {"row": 0, "column": 0, "cell_width": 5, "text": "error", "style_id": 1},
            {"row": 0, "column": 8, "cell_width": 2, "text": "中", "style_id": 0},
            {"row": 1, "column": 0, "cell_width": 2, "text": "$ ", "style_id": 2},
        ],
    })
}

enum Answer {
    Line(String),
    Refuse(String),
    Stream(mpsc::UnboundedReceiver<String>),
}

pub struct FakeState {
    pub password: String,
    pub calls: Vec<Value>,
    pub auth_lines: Vec<String>,
    pub text: String,
    pub text2: String,
    pub sidebar_states: HashMap<String, Vec<String>>,
    pub notifications: Vec<Value>,
    pub chat_sessions: Vec<Value>,
    pub chat_send_lost: bool,
    pub screenshot: String,
    /// Mimic cmuxOnly refusing a process outside cmux's process tree.
    pub deny: bool,
    /// Mimic cmux builds (0.65.x) without surface.wait_text_change.
    pub no_wait_method: bool,
    pub streams: Vec<mpsc::UnboundedSender<String>>,
}

/// Speaks the subset of the cmux v2 socket protocol psycommux uses.
#[derive(Clone)]
pub struct FakeCmux {
    pub path: String,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeCmux {
    pub fn start(path: &str, password: &str) -> Self {
        let state = Arc::new(Mutex::new(FakeState {
            password: password.into(),
            calls: vec![],
            auth_lines: vec![],
            text: "$ echo hi\nhi\n".into(),
            text2: "server listening\n".into(),
            sidebar_states: HashMap::from([(WS_ID.to_string(), sidebar_state())]),
            notifications: vec![
                json!({"id": NOTIF_ID, "workspace_id": WS_ID, "surface_id": SURF_ID,
                       "title": "Claude is waiting", "body": "Approve Bash?", "is_read": false}),
                json!({"id": NOTIF2_ID, "workspace_id": WS_ID, "surface_id": SURF2_ID,
                       "title": "Build done", "body": "", "is_read": true}),
            ],
            chat_sessions: vec![],
            chat_send_lost: false,
            screenshot: "iVBORw0KGgo=".into(),
            deny: false,
            no_wait_method: false,
            streams: vec![],
        }));
        let listener = UnixListener::bind(path).unwrap();
        let fake = Self {
            path: path.into(),
            state,
        };
        let server = fake.clone();
        tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move { server.serve(conn).await });
            }
        });
        fake
    }

    async fn serve(&self, conn: tokio::net::UnixStream) {
        let (read, mut write) = conn.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match self.answer(line.trim()) {
                Answer::Line(reply) => {
                    if write.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
                Answer::Refuse(reply) => {
                    let _ = write.write_all(reply.as_bytes()).await;
                    return;
                }
                Answer::Stream(mut rx) => {
                    let ack = json!({"type": "ack", "boot_id": "boot", "resume": {"gap": false}});
                    if write
                        .write_all(format!("{ack}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                    while let Some(frame) = rx.recv().await {
                        if write.write_all(frame.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
            }
        }
    }

    fn answer(&self, line: &str) -> Answer {
        let mut st = self.state.lock().unwrap();
        if let Some(pw) = line.strip_prefix("auth ") {
            st.auth_lines.push(line.to_string());
            return Answer::Line(if pw == st.password {
                "OK\n".into()
            } else {
                "ERROR: bad password\n".into()
            });
        }
        if st.deny {
            return Answer::Refuse(
                "ERROR: Access denied - only processes started inside cmux can connect\n".into(),
            );
        }
        if line.starts_with("sidebar_state") {
            st.calls
                .push(json!({"method": format!("v1:{line}"), "params": {}}));
            let tab = line
                .split_once("--tab=")
                .map(|(_, t)| t.split_whitespace().next().unwrap_or(""))
                .unwrap_or("");
            let lines = st
                .sidebar_states
                .get(tab)
                .cloned()
                .unwrap_or(vec!["ERROR: Tab not found".into()]);
            return Answer::Line(lines.join("\n") + "\n");
        }
        let req: Value = serde_json::from_str(line).unwrap();
        st.calls.push(req.clone());
        let method = req["method"].as_str().unwrap().to_string();
        let id = req["id"].clone();
        if method == "events.stream" {
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            st.streams.push(tx);
            return Answer::Stream(rx);
        }
        let frame = if st.no_wait_method && method == "surface.wait_text_change" {
            json!({"id": id, "ok": false, "error": {"code": "method_not_found", "message": "Unknown method"}})
        } else {
            match result(&mut st, &method, &req["params"]) {
                Some(result) => json!({"id": id, "ok": true, "result": result}),
                None => {
                    json!({"id": id, "ok": false, "error": {"code": "not_found", "message": "Terminal surface not found"}})
                }
            }
        };
        Answer::Line(format!("{frame}\n"))
    }

    pub fn push_event(&self, frame: Value) {
        for tx in &self.state.lock().unwrap().streams {
            let _ = tx.send(format!("{frame}\n"));
        }
    }

    pub fn methods(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .map(|c| c["method"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn calls_of(&self, method: &str) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c["method"] == method)
            .map(|c| c["params"].clone())
            .collect()
    }

    pub fn last(&self, method: &str) -> Value {
        self.calls_of(method)
            .pop()
            .unwrap_or_else(|| panic!("no {method} call"))
    }

    pub fn set<F: FnOnce(&mut FakeState)>(&self, f: F) {
        f(&mut self.state.lock().unwrap());
    }
}

fn result(st: &mut FakeState, method: &str, params: &Value) -> Option<Value> {
    let home = home();
    let sid = params
        .get("surface_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(match method {
        "system.ping" => json!({"pong": true}),
        "extension.sidebar.snapshot" => json!({"window_id": WIN_ID, "workspaces": [{
            "id": WS_ID, "title": "api", "unread_count": 1,
            "latest_notification_text": "Claude is waiting: Approve Bash?",
            "description": "Ship the **API**", "custom_color": "#C0392B", "pinned": true,
            "current_directory": format!("{home}/src/api"), "branch_summary": "main",
            "git_branches": [{"branch": "main", "dirty": true}],
            "pull_request_urls": ["https://github.com/o/r/pull/7"], "listening_ports": [8787],
            "latest_conversation_message": "ship it",
        }]}),
        "notification.dismiss" => {
            let id = params["id"].clone();
            st.notifications.retain(|n| n["id"] != id);
            json!({"dismissed": 1})
        }
        "notification.mark_read" => json!({"marked_read": 1}),
        "system.tree" => {
            let surf = |id: &str, pane: &str, kind: &str, title: &str, selected: bool| {
                json!({"id": id, "ref": format!("surface:{}", &id[..1]), "type": kind, "title": title,
                       "pane_id": pane, "selected_in_pane": selected, "url": null})
            };
            json!({"windows": [{"id": WIN_ID, "ref": "window:1", "workspaces": [{
                "id": WS_ID, "ref": "workspace:1", "title": "api", "selected": true,
                "layout": {"direction": "horizontal", "split": 0.4, "children": [
                    {"pane": {"id": PANE1_ID}}, {"pane": {"id": PANE2_ID}}]},
                "panes": [
                    {"id": PANE1_ID, "ref": "pane:1", "focused": true, "selected_surface_id": SURF_ID,
                     "surfaces": [surf(SURF_ID, PANE1_ID, "terminal", "zsh", true)]},
                    {"id": PANE2_ID, "ref": "pane:2", "focused": false, "selected_surface_id": SURF2_ID,
                     "surfaces": [surf(SURF2_ID, PANE2_ID, "terminal", "server", true),
                                  surf(BROWSER_ID, PANE2_ID, "browser", "docs", false)]},
                ],
            }]}]})
        }
        "terminal.replay" => {
            if sid != SURF_ID {
                return None;
            }
            json!({"surface_id": SURF_ID, "render_grid": render_grid()})
        }
        "surface.read_text" => {
            let text = if sid == SURF_ID { &st.text } else { &st.text2 };
            json!({"text": text, "surface_id": sid})
        }
        "surface.wait_text_change" => {
            let changed = params.get("after_revision").and_then(Value::as_str) != Some("rev-1");
            let text = if sid == SURF_ID { &st.text } else { &st.text2 };
            json!({"text": text, "revision": "rev-1", "changed": changed})
        }
        "notification.list" => json!({"notifications": st.notifications}),
        "browser.screenshot" => json!({"png_base64": st.screenshot, "surface_id": sid}),
        "browser.url.get" => json!({"url": "https://example.com/"}),
        "browser.eval" => json!({"value": "a"}),
        "workspace.create" => json!({"workspace_id": "new-ws", "workspace_ref": "workspace:9"}),
        "chat.sessions.dump" => json!({"sessions": st.chat_sessions}),
        "mobile.chat.send" | "mobile.chat.interrupt" if st.chat_send_lost => return None,
        _ => json!({}),
    })
}

pub struct Harness {
    pub tmp: tempfile::TempDir,
    pub fake: FakeCmux,
    pub client: Arc<CmuxClient>,
    pub hub: Arc<EventHub>,
    pub state: Arc<AppState>,
}

pub struct Reply {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(|_, _| {}).await
    }

    /// Build a harness, letting the test adjust the client and app state first.
    pub async fn with(adjust: impl FnOnce(&mut CmuxClient, &mut AppState)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cmux.sock").to_string_lossy().into_owned();
        let fake = FakeCmux::start(&path, PASSWORD);
        let mut client = CmuxClient::new(&path, Some(PASSWORD.into()));
        let mut placeholder = AppState::new(
            Arc::new(CmuxClient::new(&path, None)),
            EventHub::new(Arc::new(CmuxClient::new(&path, None))),
            TOKEN,
        );
        adjust(&mut client, &mut placeholder);
        let client = Arc::new(client);
        let hub = EventHub::new(Arc::clone(&client));
        hub.start();
        let mut state = AppState::new(Arc::clone(&client), Arc::clone(&hub), TOKEN);
        state.allowed_origins = placeholder.allowed_origins;
        state.access_log = placeholder.access_log;
        state.tls = placeholder.tls;
        state.files = placeholder.files;
        state.uploads = tmp.path().join("uploads");
        let state = Arc::new(state);
        let h = Self {
            tmp,
            fake,
            client,
            hub,
            state,
        };
        wait_until(|| h.hub.connected()).await;
        h
    }

    pub async fn send(&self, req: Request<Body>) -> Response<Body> {
        router(Arc::clone(&self.state)).oneshot(req).await.unwrap()
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
        auth: bool,
    ) -> Reply {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:8787");
        if auth {
            req = req.header("cookie", format!("{COOKIE_NAME}={TOKEN}"));
        }
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = body
            .map(|b| Body::from(b.to_string()))
            .unwrap_or_else(Body::empty);
        let resp = self.send(req.body(body).unwrap()).await;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&self, path: &str) -> Reply {
        self.request("GET", path, None, &[], true).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Reply {
        self.request(
            "POST",
            path,
            Some(body),
            &[("origin", "http://127.0.0.1:8787")],
            true,
        )
        .await
    }
}

pub async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..250 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met in time");
}

/// Reads Server-Sent Events from a response body.
pub struct Events {
    body: Body,
    buf: String,
}

impl Events {
    pub fn new(resp: Response<Body>) -> Self {
        Self {
            body: resp.into_body(),
            buf: String::new(),
        }
    }

    pub async fn next(&mut self) -> Value {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let chunk: String = self.buf.drain(..end + 2).collect();
                if let Some(data) = chunk.trim().strip_prefix("data: ") {
                    return serde_json::from_str(data).unwrap();
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(5), self.body.frame())
                .await
                .expect("no event in time");
            let frame = frame.expect("stream ended").unwrap();
            if let Ok(data) = frame.into_data() {
                self.buf.push_str(&String::from_utf8_lossy(&data));
            }
        }
    }
}

// -- a Claude Code transcript ---------------------------------------------------

pub fn jl(v: Value) -> Vec<u8> {
    let mut out = v.to_string().into_bytes();
    out.push(b'\n');
    out
}

pub fn transcript() -> Vec<u8> {
    let mut t = Vec::new();
    t.extend(
        format!(
            "{{\"type\":\"api-request-blob\",\"payload\":\"{}\"}}\n",
            "x".repeat(5000)
        )
        .into_bytes(),
    );
    t.extend(jl(json!({"parentUuid": null, "isSidechain": false, "message": {"role": "user", "content": "fix the tests"},
                       "type": "user", "uuid": "u1", "timestamp": "2026-11-02T08:00:00Z"})));
    t.extend(jl(json!({"isSidechain": false, "message": {"role": "user", "content": "<command-name>/clear</command-name>\n<command-args></command-args>"},
                       "type": "user", "uuid": "u2"})));
    t.extend(jl(json!({"isSidechain": false, "isMeta": true, "message": {"role": "user", "content": "meta"}, "type": "user", "uuid": "u3"})));
    t.extend(jl(json!({"isSidechain": false, "message": {"role": "assistant", "content": [
        {"type": "thinking", "thinking": ""},
        {"type": "text", "text": "Running **them** now."},
        {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "pytest -q", "description": "Run tests"}},
    ]}, "type": "assistant", "uuid": "a1"})));
    t.extend(jl(json!({"isSidechain": true, "message": {"role": "assistant", "content": [{"type": "text", "text": "subagent"}]},
                       "type": "assistant", "uuid": "s1"})));
    t.extend(jl(
        json!({"isSidechain": false, "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "2 failed", "is_error": true}]},
        "type": "user", "uuid": "u4"}),
    ));
    t.extend(jl(json!({"isSidechain": false, "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "t2", "name": "Edit",
         "input": {"file_path": "/src/a.py", "old_string": "a = 1\nb = 2", "new_string": "a = 1\nb = 3\nc = 4"}},
        {"type": "tool_use", "id": "t3", "name": "AskUserQuestion", "input": {"questions": [
            {"question": "Which fix?", "header": "Fix", "options": [
                {"label": "Patch", "description": "small"}, {"label": "Rewrite", "description": "big"}]}]}},
    ]}, "type": "assistant", "uuid": "a2"})));
    t
}
