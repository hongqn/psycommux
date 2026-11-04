//! End-to-end tests of the HTTP API against a fake cmux socket.

mod common;

use std::io::Read;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::Request;
use common::*;
use psycommux::cmux::CmuxClient;
use psycommux::events::parse_sidebar_state;
use psycommux::panels::SessionFiles;
use psycommux::web::COOKIE_NAME;
use serde_json::{json, Value};

// -- auth and the page --------------------------------------------------------

#[tokio::test]
async fn api_requires_token() {
    let h = Harness::new().await;
    assert_eq!(
        h.request("GET", "/api/tree", None, &[], false).await.status,
        401
    );
    let body = json!({"surface_id": SURF_ID, "text": "x"});
    assert_eq!(
        h.request("POST", "/api/send", Some(body), &[], false)
            .await
            .status,
        401
    );
    let bad = h
        .request(
            "GET",
            "/api/tree",
            None,
            &[("cookie", &format!("{COOKIE_NAME}=nope"))],
            false,
        )
        .await;
    assert_eq!(bad.status, 401);
    assert!(!h.fake.methods().contains(&"surface.send_text".to_string()));
}

#[tokio::test]
async fn login_sets_lax_http_only_cookie() {
    let h = Harness::new().await;
    let resp = h
        .request("GET", &format!("/login?token={TOKEN}"), None, &[], false)
        .await;
    assert_eq!(resp.status, 200);
    let cookie = resp.header("set-cookie").unwrap();
    assert!(cookie.contains(&format!("{COOKIE_NAME}={TOKEN}")));
    assert!(cookie.contains("HttpOnly"));
    // Lax, not Strict: an iOS Home Screen app launched cold opens "/" as if
    // from outside the site, and would get the login page without the cookie.
    assert!(cookie.contains("SameSite=Lax"));
    assert!(!cookie.contains("Secure"));
    assert_eq!(
        h.request("GET", "/login?token=wrong", None, &[], false)
            .await
            .status,
        401
    );
    let proxied = h
        .request(
            "GET",
            &format!("/login?token={TOKEN}"),
            None,
            &[("x-forwarded-proto", "https")],
            false,
        )
        .await;
    assert!(proxied.header("set-cookie").unwrap().contains("; Secure"));
}

#[tokio::test]
async fn login_continues_with_a_same_site_navigation() {
    // Opening the link from the iPhone camera is a cross-site navigation, and an
    // HTTP redirect stays part of it; finishing on a page-started navigation
    // keeps the cookie from depending on how the browser treats that redirect.
    let h = Harness::new().await;
    let resp = h
        .request("GET", &format!("/login?token={TOKEN}"), None, &[], false)
        .await;
    assert_eq!(resp.status, 200);
    assert!(resp.header("location").is_none());
    assert!(resp
        .text()
        .contains(r#"http-equiv="refresh" content="0;url=/""#));
    assert!(resp.text().contains(r#"location.replace("/")"#));
    assert_eq!(resp.header("cache-control").as_deref(), Some("no-store"));
    assert_eq!(
        resp.header("referrer-policy").as_deref(),
        Some("no-referrer")
    );
}

#[tokio::test]
async fn home_screen_manifest_and_icons_need_no_login() {
    let h = Harness::new().await;
    let resp = h
        .request("GET", "/manifest.webmanifest", None, &[], false)
        .await;
    assert_eq!(resp.status, 200);
    let manifest = resp.json();
    assert_eq!(manifest["display"], "standalone");
    let mut icons: Vec<(String, u32)> = manifest["icons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["src"].as_str().unwrap().to_string(),
                i["sizes"]
                    .as_str()
                    .unwrap()
                    .split('x')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
        })
        .collect();
    icons.push(("/apple-touch-icon.png".into(), 180));
    for (src, side) in icons {
        let resp = h.request("GET", &src, None, &[], false).await;
        assert_eq!(resp.status, 200);
        assert_eq!(&resp.body[..8], b"\x89PNG\r\n\x1a\n");
        let w = u32::from_be_bytes(resp.body[16..20].try_into().unwrap());
        let ht = u32::from_be_bytes(resp.body[20..24].try_into().unwrap());
        assert_eq!((w, ht), (side, side));
    }
}

#[tokio::test]
async fn page_without_login_shows_the_sign_in_form() {
    let h = Harness::new().await;
    let resp = h.request("GET", "/", None, &[], false).await;
    assert_eq!(resp.status, 200);
    assert!(resp.text().contains("action=/login"));
}

#[tokio::test]
async fn bearer_token_works_for_scripts() {
    let h = Harness::new().await;
    let resp = h
        .request(
            "GET",
            "/api/tree",
            None,
            &[("authorization", &format!("Bearer {TOKEN}"))],
            false,
        )
        .await;
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn socket_password_is_sent() {
    let h = Harness::new().await;
    h.get("/api/tree").await;
    assert!(h
        .fake
        .state
        .lock()
        .unwrap()
        .auth_lines
        .contains(&format!("auth {PASSWORD}")));
}

#[tokio::test]
async fn assets_and_csp() {
    let h = Harness::new().await;
    let js = h.get("/assets/app.js").await;
    assert_eq!(js.status, 200);
    assert!(js.header("content-type").unwrap().contains("javascript"));
    assert_eq!(h.get("/assets/..%2Fserver.rs").await.status, 404);
    assert_eq!(h.get("/assets/main.rs").await.status, 404);
    let page = h.get("/").await;
    assert!(page
        .header("content-security-policy")
        .unwrap()
        .contains("img-src 'self' data:"));
    assert!(page.text().contains("/assets/app.js"));
}

// -- tree and terminals -------------------------------------------------------

#[tokio::test]
async fn tree_has_layout_panes_and_tabs() {
    let h = Harness::new().await;
    let tree = h.get("/api/tree").await.json();
    let ws = &tree["windows"][0]["workspaces"][0];
    assert_eq!(ws["id"], WS_ID);
    assert_eq!(ws["directory"], "~/src/api");
    assert_eq!(ws["layout"]["direction"], "horizontal");
    assert_eq!(ws["layout"]["split"], 0.4);
    let panes: Vec<&Value> = ws["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["id"])
        .collect();
    assert_eq!(panes, [PANE1_ID, PANE2_ID]);
    let kinds: Vec<&Value> = ws["panes"][1]["surfaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| &s["type"])
        .collect();
    assert_eq!(kinds, ["terminal", "browser"]);
    assert_eq!(ws["panes"][1]["selected_surface_id"], SURF2_ID);
    assert!(!h.fake.methods().contains(&"surface.list".to_string()));
}

#[tokio::test]
async fn send_text_then_enter() {
    let h = Harness::new().await;
    let resp = h
        .post(
            "/api/send",
            json!({"surface_id": SURF_ID, "text": "ls", "enter": true}),
        )
        .await;
    assert_eq!(resp.status, 200);
    let sends: Vec<Value> = h
        .fake
        .state
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c["method"].as_str().unwrap().starts_with("surface.send"))
        .cloned()
        .collect();
    assert_eq!(sends[0]["method"], "surface.send_text");
    assert_eq!(
        sends[0]["params"],
        json!({"surface_id": SURF_ID, "text": "ls"})
    );
    assert_eq!(
        sends[1]["params"],
        json!({"surface_id": SURF_ID, "key": "enter"})
    );
}

#[tokio::test]
async fn cross_origin_post_is_rejected() {
    let h = Harness::new().await;
    let body = json!({"surface_id": SURF_ID, "key": "enter"});
    let resp = h
        .request(
            "POST",
            "/api/key",
            Some(body),
            &[("origin", "https://evil.example")],
            true,
        )
        .await;
    assert_eq!(resp.status, 403);
    assert!(!h.fake.methods().contains(&"surface.send_key".to_string()));
}

#[tokio::test]
async fn allowed_origin_may_post() {
    let h =
        Harness::with(|_, state| state.allowed_origins = vec!["https://mac.ts.net".into()]).await;
    let body = json!({"surface_id": SURF_ID, "key": "enter"});
    let resp = h
        .request(
            "POST",
            "/api/key",
            Some(body),
            &[("origin", "https://mac.ts.net")],
            true,
        )
        .await;
    assert_eq!(resp.status, 200);
}

#[tokio::test]
async fn post_requires_json_content_type() {
    let h = Harness::new().await;
    let req = Request::post("/api/key")
        .header("cookie", format!("{COOKIE_NAME}={TOKEN}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("surface_id=x&key=enter"))
        .unwrap();
    assert_eq!(h.send(req).await.status(), 400);
}

async fn screens(h: &Harness, revisions: Value) -> Value {
    let resp = h.post("/api/screens", json!({"surfaces": revisions})).await;
    assert_eq!(resp.status, 200, "{}", resp.text());
    resp.json()["screens"].clone()
}

#[tokio::test]
async fn single_screen_long_polls_with_native_wait() {
    let h = Harness::new().await;
    let first = &screens(&h, json!({SURF_ID: ""})).await[SURF_ID];
    assert_eq!(first["text"], "$ echo hi\nhi\n");
    let again = screens(&h, json!({SURF_ID: "rev-1"})).await[SURF_ID].clone();
    assert_eq!(again["changed"], false);
    assert!(again.get("grid").is_none());
    let waits = h.fake.calls_of("surface.wait_text_change");
    assert!(waits[0].get("after_revision").is_none());
    assert_eq!(waits[1]["after_revision"], "rev-1");
    assert_eq!(waits[1]["timeout_ms"], 25000);
}

#[tokio::test]
async fn screen_carries_colored_grid() {
    let h = Harness::new().await;
    let grid = screens(&h, json!({SURF_ID: ""})).await[SURF_ID]["grid"].clone();
    assert_eq!(
        (grid["columns"].clone(), grid["rows"].clone()),
        (json!(20), json!(2))
    );
    assert_eq!(grid["lines"].as_array().unwrap().len(), 3);
    assert_eq!(grid["lines"][0], json!([[0, 3, "old", 0]]));
    assert_eq!(
        grid["lines"][1],
        json!([[0, 5, "error", 1], [8, 2, "中", 0]])
    );
    let style = |id: &str| grid["styles"][id].as_str().unwrap().to_string();
    assert!(style("1").contains("color:#CC372E"));
    assert!(style("1").contains("font-weight:bold"));
    assert_eq!(style("0"), "");
    assert!(style("2").contains("color:#1E1E1E"));
    assert!(style("2").contains("background:#FFFFFF"));
    assert_eq!(
        grid["cursor"],
        json!({"line": 2, "column": 4, "visible": true})
    );
    assert_eq!(
        (grid["foreground"].clone(), grid["background"].clone()),
        (json!("#FFFFFF"), json!("#1E1E1E"))
    );
}

#[tokio::test]
async fn screen_without_grid_falls_back_to_text() {
    let h = Harness::new().await;
    let screen = screens(&h, json!({SURF2_ID: ""})).await[SURF2_ID].clone();
    assert_eq!(screen["text"], "server listening\n");
    assert!(screen.get("grid").is_none());
}

#[tokio::test]
async fn screens_answers_several_surfaces() {
    let h = Harness::new().await;
    let result = screens(&h, json!({SURF_ID: "", SURF2_ID: ""})).await;
    let obj = result.as_object().unwrap();
    assert_eq!(obj.len(), 2);
    assert!(obj.values().all(|r| r["changed"] == true));
}

#[tokio::test]
async fn screens_gzip() {
    let h = Harness::new().await;
    h.fake.set(|st| st.text = "x".repeat(4096));
    let resp = h
        .request(
            "POST",
            "/api/screens",
            Some(json!({"surfaces": {SURF_ID: ""}})),
            &[("accept-encoding", "gzip")],
            true,
        )
        .await;
    assert_eq!(resp.header("content-encoding").as_deref(), Some("gzip"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(&resp.body[..])
        .read_to_string(&mut text)
        .unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert!(body["screens"].get(SURF_ID).is_some());
}

#[tokio::test]
async fn screens_rejects_bad_input() {
    let h = Harness::new().await;
    assert_eq!(
        h.post("/api/screens", json!({"surfaces": []})).await.status,
        400
    );
    let many: serde_json::Map<String, Value> = (0..13)
        .map(|i| (format!("{i:08}-0000-0000-0000-000000000000"), json!("")))
        .collect();
    assert_eq!(
        h.post("/api/screens", json!({"surfaces": many}))
            .await
            .status,
        400
    );
}

// -- events and the sidebar ---------------------------------------------------

async fn open_events(h: &Harness) -> Events {
    let req = Request::get("/api/events")
        .header("cookie", format!("{COOKIE_NAME}={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.send(req).await;
    assert_eq!(resp.status(), 200);
    let mut events = Events::new(resp);
    assert_eq!(events.next().await["type"], "status");
    events
}

#[tokio::test]
async fn sse_delivers_unredacted_notification() {
    let h = Harness::new().await;
    let mut events = open_events(&h).await;
    h.fake.push_event(
        json!({"type": "event", "seq": 5, "name": "surface.input_sent", "category": "surface"}),
    );
    h.fake.push_event(json!({
        "type": "event", "seq": 6, "name": "notification.created", "category": "notification",
        "workspace_id": WS_ID, "surface_id": SURF_ID,
        "payload": {"notification_id": NOTIF_ID, "title": null, "body": null,
                    "redacted_fields": ["title", "subtitle", "body"]},
    }));
    let msg = events.next().await;
    assert_eq!(msg["name"], "notification.created");
    assert_eq!(msg["notification"]["title"], "Claude is waiting");
    assert_eq!(h.hub.last_seq(), Some(6));
}

#[tokio::test]
async fn parse_sidebar_state_like_the_cmux_sidebar() {
    let state = parse_sidebar_state(&sidebar_state());
    let keys: Vec<&Value> = state["statuses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["key"])
        .collect();
    assert_eq!(keys, ["program_status", "claude_code", "docs"]);
    let working = &state["statuses"][0];
    assert_eq!(working["value"], "claude-code · Working");
    assert_eq!(
        (working["icon"].clone(), working["priority"].clone()),
        (json!("bolt.fill"), json!(2))
    );
    let running = &state["statuses"][1];
    assert_eq!(
        (running["color"].clone(), running["work"].clone()),
        (json!("#4C8DFF"), json!("running"))
    );
    let docs = &state["statuses"][2];
    assert_eq!(
        (docs["format"].clone(), docs["url"].clone()),
        (json!("markdown"), json!("https://example.com/spec"))
    );
    assert_eq!(
        state["log"],
        json!({"level": "success", "message": "合并。 latest wins"})
    );
    assert_eq!(
        state["progress"],
        json!({"value": 0.4, "label": "Indexing"})
    );
    assert_eq!(state["working"], true);
    assert_eq!(state["git_dirty"], true);

    let empty = parse_sidebar_state(&["tab=x", "progress=none", "status_count=0", "log_count=0"]);
    assert_eq!(
        (
            empty["statuses"].clone(),
            empty["log"].clone(),
            empty["progress"].clone(),
            empty["working"].clone()
        ),
        (json!([]), Value::Null, Value::Null, json!(false))
    );
}

#[tokio::test]
async fn v1_reply_reads_to_the_end_of_the_log() {
    let h = Harness::new().await;
    let started = Instant::now();
    let lines = h
        .client
        .v1_lines(&format!("sidebar_state --tab={WS_ID}"))
        .await
        .unwrap();
    assert_eq!(lines, sidebar_state());
    // Ends on the log section, not on an idle timeout.
    assert!(started.elapsed() < Duration::from_millis(120));
}

#[tokio::test]
async fn tree_rows_match_the_cmux_sidebar() {
    let h = Harness::new().await;
    h.hub.sidebar.fetch(WS_ID).await.unwrap();
    let ws = h.get("/api/tree").await.json()["windows"][0]["workspaces"][0].clone();
    assert_eq!(ws["unread_count"], 1);
    assert_eq!(
        ws["latest_notification_text"],
        "Claude is waiting: Approve Bash?"
    );
    assert_eq!(ws["latest_message"], "ship it");
    assert_eq!(ws["description"], "Ship the **API**");
    assert_eq!(ws["custom_color"], "#C0392B");
    assert_eq!(ws["pinned"], true);
    assert_eq!(ws["directory"], "~/src/api");
    assert_eq!(ws["branch"], "main");
    assert_eq!(ws["git_dirty"], true);
    assert_eq!(
        ws["pull_requests"],
        json!(["https://github.com/o/r/pull/7"])
    );
    assert_eq!(ws["ports"], json!([8787]));
    assert_eq!(
        ws["sidebar"]["statuses"][0]["value"],
        "claude-code · Working"
    );
    assert_eq!(ws["sidebar"]["log"]["message"], "合并。 latest wins");
}

#[tokio::test]
async fn tree_fetches_missing_sidebar_state_in_background() {
    let h = Harness::new().await;
    let ws = h.get("/api/tree").await.json()["windows"][0]["workspaces"][0].clone();
    assert!(ws["sidebar"].is_null());
    wait_until(|| h.hub.sidebar.get(WS_ID).is_some()).await;
}

#[tokio::test]
async fn sidebar_event_refreshes_row_and_notifies_browser() {
    let h = Harness::new().await;
    h.hub.sidebar.fetch(WS_ID).await.unwrap();
    let mut events = open_events(&h).await;
    h.fake.set(|st| {
        st.sidebar_states.insert(
            WS_ID.into(),
            vec![
                format!("tab={WS_ID}"),
                "progress=none".into(),
                "status_count=1".into(),
                "  claude_code=Idle icon=pause.circle.fill color=#8E8E93".into(),
                "log_count=0".into(),
            ],
        );
    });
    h.fake.push_event(
        json!({"type": "event", "seq": 9, "name": "program_status.changed",
                             "category": "terminal", "workspace_id": WS_ID}),
    );
    let msg = events.next().await;
    assert_eq!(msg["type"], "sidebar");
    assert_eq!(msg["workspace_id"], WS_ID);
    assert_eq!(msg["sidebar"]["statuses"][0]["value"], "Idle");
    assert_eq!(msg["sidebar"]["working"], false);
    assert_eq!(
        h.hub.sidebar.get(WS_ID).unwrap()["statuses"][0]["value"],
        "Idle"
    );
}

#[tokio::test]
async fn events_subscribe_to_sidebar_and_terminal() {
    let h = Harness::new().await;
    let stream = h.fake.last("events.stream");
    let cats: Vec<&str> = stream["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(cats.contains(&"sidebar") && cats.contains(&"terminal"));
}

#[tokio::test]
async fn clear_all_dismisses_every_notification() {
    let h = Harness::new().await;
    let resp = h.post("/api/notifications/clear_all", json!({})).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.json()["dismissed"], 2);
    let mut dismissed: Vec<String> = h
        .fake
        .calls_of("notification.dismiss")
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    dismissed.sort();
    assert_eq!(dismissed, [NOTIF_ID, NOTIF2_ID]);
}

#[tokio::test]
async fn mark_read_for_a_pane() {
    let h = Harness::new().await;
    assert_eq!(
        h.post(
            "/api/notifications/read",
            json!({"workspace_id": WS_ID, "surface_id": SURF_ID})
        )
        .await
        .status,
        200
    );
    assert_eq!(
        h.fake.last("notification.mark_read"),
        json!({"workspace_id": WS_ID, "surface_id": SURF_ID})
    );
}

// -- older cmux without surface.wait_text_change ------------------------------

async fn fallback_harness() -> Harness {
    let h = Harness::with(|client, _| {
        client.wait_timeout = Duration::from_millis(1500);
        client.fallback_poll = Duration::from_millis(50);
    })
    .await;
    h.fake.set(|st| st.no_wait_method = true);
    h
}

async fn screen(h: &Harness, revision: &str) -> Value {
    screens(h, json!({SURF_ID: revision})).await[SURF_ID].clone()
}

#[tokio::test]
async fn fallback_first_read_returns_text_and_revision() {
    let h = fallback_harness().await;
    let first = screen(&h, "").await;
    assert_eq!(first["text"], "$ echo hi\nhi\n");
    assert!(!first["revision"].as_str().unwrap().is_empty());
    assert!(h.fake.methods().contains(&"surface.read_text".to_string()));
}

#[tokio::test]
async fn fallback_wait_returns_when_text_changes() {
    let h = fallback_harness().await;
    let first = screen(&h, "").await;
    let fake = h.fake.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        fake.set(|st| st.text = "new output\n".into());
    });
    let started = Instant::now();
    let second = screen(&h, first["revision"].as_str().unwrap()).await;
    assert_eq!(second["changed"], true);
    assert_eq!(second["text"], "new output\n");
    assert_ne!(second["revision"], first["revision"]);
    assert!(started.elapsed() < Duration::from_millis(1400));
}

#[tokio::test]
async fn fallback_multi_wait_returns_only_changed_surface() {
    let h = fallback_harness().await;
    let first = screens(&h, json!({SURF_ID: "", SURF2_ID: ""})).await;
    let revs: serde_json::Map<String, Value> = first
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v["revision"].clone()))
        .collect();
    let fake = h.fake.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        fake.set(|st| st.text2 = "GET /health 200\n".into());
    });
    let second = screens(&h, Value::Object(revs)).await;
    assert_eq!(second[SURF_ID]["changed"], false);
    assert_eq!(second[SURF2_ID]["changed"], true);
    assert_eq!(second[SURF2_ID]["text"], "GET /health 200\n");
}

#[tokio::test]
async fn fallback_wait_times_out_unchanged() {
    let h = fallback_harness().await;
    let first = screen(&h, "").await;
    let second = screen(&h, first["revision"].as_str().unwrap()).await;
    assert_eq!(second["changed"], false);
    assert_eq!(second["revision"], first["revision"]);
}

// -- access denied ------------------------------------------------------------

#[tokio::test]
async fn access_denied_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("cmux.sock").to_string_lossy().into_owned();
    let fake = FakeCmux::start(&path, "");
    fake.set(|st| st.deny = true);
    let client = CmuxClient::new(&path, None);
    let err = client.call("system.ping", json!({})).await.unwrap_err();
    assert_eq!(err.code, "access_denied");
    assert!(err.message.contains("inside cmux"));

    let client = std::sync::Arc::new(client);
    let hub = psycommux::events::EventHub::new(std::sync::Arc::clone(&client));
    let state = std::sync::Arc::new(psycommux::web::AppState::new(client, hub, "t"));
    let req = Request::get("/api/tree")
        .header("authorization", "Bearer t")
        .body(Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(psycommux::web::router(state), req)
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["error"],
        "access_denied"
    );
}

// -- agent chat ---------------------------------------------------------------

async fn chat_harness() -> (Harness, std::path::PathBuf) {
    let h = Harness::new().await;
    let path = h.tmp.path().join("session.jsonl");
    std::fs::write(&path, transcript()).unwrap();
    let p = path.to_string_lossy().into_owned();
    h.fake.set(|st| {
        st.chat_sessions = vec![
            json!({"session_id": "sess-1", "agent": "claude", "surface_id": SURF_ID.to_uppercase(), "workspace_id": WS_ID,
                   "transcript_path": p, "state": "working(since: 2026-11-02 08:45:00 +0000)", "last_activity": 2.0}),
            json!({"session_id": "old", "agent": "claude", "surface_id": SURF_ID, "workspace_id": WS_ID,
                   "transcript_path": p, "state": "ended", "last_activity": 9.0}),
            json!({"session_id": "codex-1", "agent": "codex", "surface_id": SURF2_ID, "workspace_id": WS_ID,
                   "transcript_path": p, "state": "idle"}),
        ]
    });
    (h, path)
}

#[tokio::test]
async fn tree_marks_agent_terminals() {
    let (h, path) = chat_harness().await;
    let tree = h.get("/api/tree").await.json();
    let mut surfaces = std::collections::HashMap::new();
    for pane in tree["windows"][0]["workspaces"][0]["panes"]
        .as_array()
        .unwrap()
    {
        for s in pane["surfaces"].as_array().unwrap() {
            surfaces.insert(s["id"].as_str().unwrap().to_string(), s.clone());
        }
    }
    assert_eq!(
        surfaces[SURF_ID]["agent"],
        json!({"session_id": "sess-1", "agent": "claude", "state": "working", "chat": true})
    );
    assert_eq!(surfaces[SURF2_ID]["agent"]["chat"], false);
    assert!(surfaces[BROWSER_ID]["agent"].is_null());
    assert!(!tree.to_string().contains(&*path.to_string_lossy()));
}

#[tokio::test]
async fn chat_history_and_poll() {
    let (h, path) = chat_harness().await;
    let page = h
        .post("/api/chat/history", json!({"session_id": "sess-1"}))
        .await;
    assert_eq!(page.status, 200);
    let page = page.json();
    assert_eq!(page["state"], "working");
    assert_eq!(page["items"].as_array().unwrap().len(), 7);
    let end = page["end"].as_u64().unwrap();
    let poll = h.post(
        "/api/chat/poll",
        json!({"session_id": "sess-1", "after": end, "state": "working"}),
    );
    let writer = async {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let line = jl(
            json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "All green."}]},
                             "type": "assistant", "uuid": "a9"}),
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, &line).unwrap();
        Instant::now()
    };
    let (update, written) = tokio::join!(poll, writer);
    assert!(
        Instant::now() >= written,
        "poll answered before the transcript grew"
    );
    let update = update.json();
    let texts: Vec<&Value> = update["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["text"])
        .collect();
    assert_eq!(texts, ["All green."]);
}

#[tokio::test]
async fn chat_poll_returns_when_state_changes() {
    let (h, _) = chat_harness().await;
    h.fake.set(|st| {
        st.chat_sessions[0]["state"] = json!("needsInput(since: 2026-11-02 08:46:00 +0000)")
    });
    let after = transcript().len();
    let update = h
        .post(
            "/api/chat/poll",
            json!({"session_id": "sess-1", "after": after, "state": "working"}),
        )
        .await;
    assert_eq!(update.status, 200);
    assert_eq!(update.json()["state"], "needs_input");
    assert_eq!(update.json()["items"], json!([]));
}

#[tokio::test]
async fn chat_unknown_and_unsupported_sessions() {
    let (h, _) = chat_harness().await;
    assert_eq!(
        h.post("/api/chat/history", json!({"session_id": "nope"}))
            .await
            .status,
        404
    );
    assert_eq!(
        h.post("/api/chat/history", json!({"session_id": "codex-1"}))
            .await
            .status,
        409
    );
}

#[tokio::test]
async fn chat_send_goes_through_mobile_chat_send() {
    let (h, _) = chat_harness().await;
    assert_eq!(
        h.post(
            "/api/chat/send",
            json!({"session_id": "sess-1", "text": "line one\nline two"})
        )
        .await
        .status,
        200
    );
    assert_eq!(
        h.fake.last("mobile.chat.send"),
        json!({"session_id": "sess-1", "text": "line one\nline two"})
    );
}

#[tokio::test]
async fn chat_send_falls_back_to_paste_when_binding_is_lost() {
    let (h, _) = chat_harness().await;
    h.fake.set(|st| st.chat_send_lost = true);
    assert_eq!(
        h.post(
            "/api/chat/send",
            json!({"session_id": "sess-1", "text": "hi"})
        )
        .await
        .status,
        200
    );
    let calls = h.fake.state.lock().unwrap().calls.clone();
    let tail: Vec<(String, String)> = calls[calls.len() - 4..]
        .iter()
        .map(|c| {
            let p = &c["params"];
            let v = p
                .get("key")
                .or_else(|| p.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (c["method"].as_str().unwrap().to_string(), v.to_string())
        })
        .collect();
    let expect = [
        ("surface.send_key", "ctrl+a"),
        ("surface.send_key", "ctrl+k"),
        ("surface.send_key", "ctrl+u"),
        ("terminal.paste", "hi"),
    ];
    assert_eq!(tail, expect.map(|(a, b)| (a.to_string(), b.to_string())));
}

#[tokio::test]
async fn chat_interrupt() {
    let (h, _) = chat_harness().await;
    assert_eq!(
        h.post("/api/chat/interrupt", json!({"session_id": "sess-1"}))
            .await
            .status,
        200
    );
    assert_eq!(
        h.fake.last("mobile.chat.interrupt"),
        json!({"session_id": "sess-1", "hard": false})
    );
}

// -- browsers, files, new sessions --------------------------------------------

#[tokio::test]
async fn browser_shot_holds_the_viewport_and_skips_unchanged_images() {
    let h = Harness::new().await;
    let first = h
        .post(
            "/api/browser/shot",
            json!({"surface_id": BROWSER_ID, "width": 390, "height": 700}),
        )
        .await;
    assert_eq!(first.status, 200);
    let first = first.json();
    assert_eq!(
        (first["png"].clone(), first["url"].clone()),
        (json!("iVBORw0KGgo="), json!("https://example.com/"))
    );
    assert_eq!(
        h.fake.last("browser.viewport.set"),
        json!({"surface_id": BROWSER_ID, "width": 390, "height": 700})
    );
    let sets = h.fake.calls_of("browser.viewport.set").len();
    let second = h
        .post(
            "/api/browser/shot",
            json!({"surface_id": BROWSER_ID, "hash": first["hash"], "width": 390, "height": 700}),
        )
        .await
        .json();
    assert!(second.get("png").is_none());
    assert_eq!(h.fake.calls_of("browser.viewport.set").len(), sets);
    h.post("/api/browser/release", json!({"surface_id": BROWSER_ID}))
        .await;
    assert_eq!(
        h.fake.last("browser.viewport.set"),
        json!({"surface_id": BROWSER_ID, "reset": true})
    );
}

#[tokio::test]
async fn browser_actions() {
    let h = Harness::new().await;
    let act = |body: Value| {
        let mut body = body;
        body["surface_id"] = json!(BROWSER_ID);
        h.post("/api/browser/act", body)
    };
    assert_eq!(
        act(json!({"action": "tap", "fx": 0.5, "fy": 0.25}))
            .await
            .status,
        200
    );
    let script = h.fake.last("browser.eval")["script"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(script.contains("elementFromPoint"));
    assert!(script.contains("0.250000 * innerHeight"));
    act(json!({"action": "type", "text": "it's \"quoted\""})).await;
    assert!(h.fake.last("browser.eval")["script"]
        .as_str()
        .unwrap()
        .contains(r#""it's \"quoted\"""#));
    act(json!({"action": "navigate", "url": "example.org/x"})).await;
    assert_eq!(
        h.fake.last("browser.navigate")["url"],
        "https://example.org/x"
    );
    act(json!({"action": "navigate", "url": "localhost:8080"})).await;
    assert_eq!(
        h.fake.last("browser.navigate")["url"],
        "https://localhost:8080"
    );
    for bad in [
        json!({"action": "navigate", "url": "javascript:alert(1)"}),
        json!({"action": "navigate", "url": "file:///etc/passwd"}),
        json!({"action": "key", "key": "Meta"}),
        json!({"action": "tap", "fx": "0.5", "fy": 0.1}),
        json!({"action": "explode"}),
    ] {
        assert_eq!(act(bad.clone()).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn file_panels_read_the_path_cmux_saved() {
    let tmp = tempfile::tempdir().unwrap();
    let doc = tmp.path().join("notes.md");
    std::fs::write(&doc, "# Notes\nhello").unwrap();
    let session = json!({"windows": [{"panels": [{"id": BROWSER_ID.to_lowercase(), "type": "markdown", "markdown": {"filePath": doc}}]}]});
    std::fs::write(
        tmp.path().join("session-com.cmuxterm.app.json"),
        session.to_string(),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("session-com.cmuxterm.app-previous.json"),
        "{}",
    )
    .unwrap();
    let pattern = tmp
        .path()
        .join("session-com.cmuxterm.app*.json")
        .to_string_lossy()
        .into_owned();
    let h = Harness::with(|_, state| state.files = SessionFiles::new(pattern)).await;
    let first = h.post("/api/file", json!({"surface_id": BROWSER_ID})).await;
    assert_eq!(first.status, 200);
    let first = first.json();
    assert_eq!(
        (first["text"].clone(), first["name"].clone()),
        (json!("# Notes\nhello"), json!("notes.md"))
    );
    let again = h
        .post(
            "/api/file",
            json!({"surface_id": BROWSER_ID, "stamp": first["stamp"]}),
        )
        .await
        .json();
    assert_eq!(again, json!({"unchanged": true, "stamp": first["stamp"]}));
    assert_eq!(
        h.post("/api/file", json!({"surface_id": SURF_ID}))
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn new_session_starts_claude_with_the_prompt() {
    let h = Harness::new().await;
    let r = h
        .post(
            "/api/session/new",
            json!({"cwd": "~/src/api", "prompt": "fix it; rm -rf /"}),
        )
        .await;
    assert_eq!(
        (r.status, r.json()["workspace_id"].clone()),
        (200, json!("new-ws"))
    );
    assert_eq!(
        h.fake.last("workspace.create"),
        json!({"cwd": "~/src/api", "initial_input": "claude 'fix it; rm -rf /'\n"})
    );
    h.post("/api/session/new", json!({"cwd": "/tmp", "prompt": "it's"}))
        .await;
    assert_eq!(
        h.fake.last("workspace.create")["initial_input"],
        "claude 'it'\"'\"'s'\n"
    );
    h.post("/api/session/new", json!({"cwd": "/tmp"})).await;
    assert_eq!(h.fake.last("workspace.create")["initial_input"], "claude\n");
}

// -- HTTPS --------------------------------------------------------------------

#[tokio::test]
async fn https_login_sets_a_secure_cookie_and_stalled_clients_do_not_block() {
    let h = Harness::with(|_, state| state.tls = true).await;
    let dir = h.tmp.path();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let made = std::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=mac.tail1.ts.net",
            "-addext",
            "subjectAltName=DNS:mac.tail1.ts.net",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output();
    if !made.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: needs the openssl command");
        return;
    }
    let config = psycommux::net::load_tls_config(&cert, Some(&key)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = psycommux::web::router(std::sync::Arc::clone(&h.state));
    tokio::spawn(psycommux::net::serve_tls(listener, app, config));

    // Connects, never handshakes.
    let _stuck = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let curl = |path: &str, extra: &[&str]| {
        let mut cmd = tokio::process::Command::new("curl");
        cmd.args([
            "-sS",
            "--noproxy",
            "*",
            "-m",
            "5",
            "-D",
            "-",
            "-o",
            "/dev/null",
            "--cacert",
        ])
        .arg(&cert)
        .args(["--resolve", &format!("mac.tail1.ts.net:{port}:127.0.0.1")])
        .args(extra)
        .arg(format!("https://mac.tail1.ts.net:{port}{path}"));
        cmd.output()
    };
    let login = curl(&format!("/login?token={TOKEN}"), &[]).await.unwrap();
    let headers = String::from_utf8_lossy(&login.stdout).to_lowercase();
    assert!(
        headers.starts_with("http/"),
        "{headers} {}",
        String::from_utf8_lossy(&login.stderr)
    );
    assert!(headers.contains("; secure"), "{headers}");
    let tree = curl(
        "/api/tree",
        &["-H", &format!("Authorization: Bearer {TOKEN}")],
    )
    .await
    .unwrap();
    let headers = String::from_utf8_lossy(&tree.stdout);
    assert!(
        headers.split_whitespace().nth(1) == Some("200"),
        "{headers}"
    );
}
