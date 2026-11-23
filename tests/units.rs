//! Tests of the pure parts: transcripts, phone URLs, QR codes and TLS files.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use psycommux::agentchat::{self, read_since, read_tail, session_state};
use psycommux::net::{
    self, cert_hostname, default_tls_key, load_tls_config, origin_of, phone_url,
    tailscale_serve_url,
};
use serde_json::{json, Value};

mod common;
use common::{jl, transcript};

fn write_transcript(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("t.jsonl");
    std::fs::write(&path, transcript()).unwrap();
    path
}

fn label(item: &Value) -> String {
    for key in ["text", "tool", "prompt"] {
        if let Some(s) = item
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return s.to_string();
        }
    }
    String::new()
}

#[test]
fn transcript_becomes_chat_items() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_transcript(tmp.path());
    let page = read_tail(&path, None, 80, agentchat::CHUNK).unwrap();
    let items = page["items"].as_array().unwrap();
    let kinds: Vec<(String, String)> = items
        .iter()
        .map(|i| (i["kind"].as_str().unwrap().to_string(), label(i)))
        .collect();
    let expect = [
        ("user", "fix the tests"),
        ("user", "/clear"),
        ("assistant", "Running **them** now."),
        ("tool", "Bash"),
        ("result", "2 failed"),
        ("tool", "Edit"),
        ("question", "Which fix?"),
    ];
    assert_eq!(kinds, expect.map(|(a, b)| (a.to_string(), b.to_string())));
    let (bash, result, edit, question) = (&items[3], &items[4], &items[5], &items[6]);
    assert_eq!(
        (
            bash["id"].clone(),
            bash["summary"].clone(),
            bash["note"].clone()
        ),
        (json!("t1"), json!("pytest -q"), json!("Run tests"))
    );
    assert_eq!(
        (result["call_id"].clone(), result["is_error"].clone()),
        (json!("t1"), json!(true))
    );
    assert_eq!(
        (
            edit["path"].clone(),
            edit["additions"].clone(),
            edit["deletions"].clone()
        ),
        (json!("/src/a.py"), json!(2), json!(1))
    );
    assert!(edit["diff"]
        .as_str()
        .unwrap()
        .starts_with("@@ -1,2 +1,3 @@"));
    let labels: Vec<&Value> = question["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| &o["label"])
        .collect();
    assert_eq!(labels, ["Patch", "Rewrite"]);
    assert_eq!(question["call_id"], "t3");
    assert_eq!(page["end"], transcript().len());
}

#[test]
fn pages_backwards_and_reads_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_transcript(tmp.path());
    let last = read_tail(&path, None, 1, 300).unwrap();
    let start = last["start"].as_u64().unwrap();
    assert!(start > 0);
    let earlier = read_tail(&path, Some(start), 80, 300).unwrap();
    assert_eq!(earlier["start"], 0);
    let ids = |v: &Value| {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].clone())
            .collect::<Vec<_>>()
    };
    let mut joined = ids(&earlier);
    joined.extend(ids(&last));
    assert_eq!(
        joined,
        ids(&read_tail(&path, None, 80, agentchat::CHUNK).unwrap())
    );

    let line = jl(
        json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                         "type": "assistant", "uuid": "a3"}),
    );
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&line).unwrap();
    f.write_all(br#"{"type":"assistant","partial"#).unwrap(); // a line still being written
    let update = read_since(&path, transcript().len() as u64).unwrap();
    let texts: Vec<&Value> = update["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["text"])
        .collect();
    assert_eq!(texts, ["done"]);
    assert_eq!(update["end"], transcript().len() + line.len());
}

#[test]
fn long_final_line_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_transcript(tmp.path());
    let big = jl(
        json!({"message": {"role": "assistant", "content": [{"type": "text", "text": "y".repeat(2000)}]},
                        "type": "assistant", "uuid": "big"}),
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&big)
        .unwrap();
    let page = read_tail(&path, None, 80, 300).unwrap();
    assert_eq!(
        page["items"].as_array().unwrap().last().unwrap()["id"],
        "big:0"
    );
    assert_eq!(page["end"], transcript().len() + big.len());
}

#[test]
fn truncated_transcript_resets() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_transcript(tmp.path());
    let update = read_since(&path, 10 * transcript().len() as u64).unwrap();
    assert_eq!(update["reset"], true);
    assert_eq!(update["items"].as_array().unwrap().len(), 7);
}

#[test]
fn session_states() {
    assert_eq!(
        session_state("working(since: 2026-11-02 08:45:00 +0000)"),
        "working"
    );
    assert_eq!(
        session_state("needsInput(since: 2026-11-02 08:45:00 +0000)"),
        "needs_input"
    );
    assert_eq!(session_state("idle"), "idle");
    assert_eq!(session_state("ended"), "ended");
    assert_eq!(session_state(""), "unknown");
}

#[test]
fn qr_code_of_a_login_url() {
    let url = format!(
        "https://mac.tailnet.ts.net/login?token={}",
        "x".repeat(32)
    );
    let qr = net::qr_render(&url).unwrap();
    let code =
        qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::M).unwrap();
    let n = code.width();
    assert_eq!((n - 17) % 4, 0);
    assert_eq!(qr.lines().count(), (n + 8).div_ceil(2));
    // The top quiet zone is blank; the finder pattern starts below it.
    assert!(qr.lines().nth(1).unwrap().contains('█') || qr.lines().nth(2).unwrap().contains('█'));
    assert!(net::qr_render(&"x".repeat(4000)).is_err());
}

#[test]
fn tailscale_serve_url_finds_our_proxy() {
    let status = json!({"TCP": {"443": {"HTTPS": true}},
                        "Web": {"mac.tail1.ts.net:443": {"Handlers": {"/": {"Proxy": "http://127.0.0.1:8787"}}}}});
    assert_eq!(
        tailscale_serve_url(8787, &status).as_deref(),
        Some("https://mac.tail1.ts.net")
    );
    assert_eq!(tailscale_serve_url(9999, &status), None);
    let other =
        json!({"Web": {"mac.tail1.ts.net:8443": {"Handlers": {"/": {"Proxy": "localhost:8787"}}}}});
    assert_eq!(
        tailscale_serve_url(8787, &other).as_deref(),
        Some("https://mac.tail1.ts.net:8443")
    );
    assert_eq!(tailscale_serve_url(8787, &Value::Null), None);
}

#[test]
fn phone_urls() {
    assert_eq!(phone_url("127.0.0.1", 8787, None, None, None, "http"), None);
    assert_eq!(phone_url("0.0.0.0", 8787, None, None, None, "http"), None);
    assert_eq!(
        phone_url("100.1.2.3", 8787, None, None, None, "http").as_deref(),
        Some("http://100.1.2.3:8787")
    );
    assert_eq!(
        phone_url("fd7a::1", 8787, None, None, None, "http").as_deref(),
        Some("http://[fd7a::1]:8787")
    );
    assert_eq!(
        phone_url(
            "127.0.0.1",
            8787,
            None,
            Some("https://m.ts.net"),
            None,
            "http"
        )
        .as_deref(),
        Some("https://m.ts.net")
    );
    assert_eq!(
        phone_url(
            "0.0.0.0",
            8787,
            Some("http://mac.local:8787/"),
            None,
            None,
            "http"
        )
        .as_deref(),
        Some("http://mac.local:8787")
    );
    assert_eq!(origin_of("https://m.ts.net/x?y"), "https://m.ts.net");
    assert_eq!(net::netloc("https://user@m.ts.net:8443/x"), "m.ts.net:8443");
}

#[test]
fn shell_quoting_matches_python() {
    assert_eq!(net::shell_quote(""), "''");
    assert_eq!(net::shell_quote("plain-word_1.2/x"), "plain-word_1.2/x");
    assert_eq!(net::shell_quote("fix it; rm -rf /"), "'fix it; rm -rf /'");
    assert_eq!(net::shell_quote("it's"), "'it'\"'\"'s'");
    assert_eq!(net::shell_quote("中文"), "'中文'");
}

#[test]
fn navigation_only_to_web_pages() {
    use psycommux::web::navigable_url;
    assert_eq!(
        navigable_url("example.org/x").unwrap(),
        "https://example.org/x"
    );
    assert_eq!(
        navigable_url("localhost:8080").unwrap(),
        "https://localhost:8080"
    );
    assert_eq!(navigable_url(" http://a.b ").unwrap(), "http://a.b");
    for bad in [
        "",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,x",
        "JAVASCRIPT:x",
    ] {
        assert!(navigable_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn tokens_are_random_and_url_safe() {
    let (a, b) = (net::new_token(), net::new_token());
    assert_ne!(a, b);
    assert_eq!(a.len(), 32);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert!(net::constant_time_eq(b"abc", b"abc"));
    assert!(!net::constant_time_eq(b"abc", b"abd"));
    assert!(!net::constant_time_eq(b"abc", b"ab"));
}

// -- TLS ----------------------------------------------------------------------

/// A self-signed cert for mac.tail1.ts.net, or None without the openssl command.
fn make_cert(dir: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let status = Command::new("openssl")
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
        .output()
        .ok()?;
    status.status.success().then_some((cert, key))
}

#[test]
fn tls_files() {
    let tmp = tempfile::tempdir().unwrap();
    let Some((cert, key)) = make_cert(tmp.path()) else {
        eprintln!("skipped: needs the openssl command");
        return;
    };
    assert!(load_tls_config(&cert, Some(&key)).is_ok());
    let err = load_tls_config(&cert, None).unwrap_err();
    assert!(err.contains("--tls-key"), "{err}");

    let crt = tmp.path().join("mac.crt");
    std::fs::copy(&cert, &crt).unwrap();
    assert_eq!(default_tls_key(&crt), None);
    std::fs::copy(&key, tmp.path().join("mac.key")).unwrap();
    assert_eq!(default_tls_key(&crt), Some(tmp.path().join("mac.key")));
    assert!(load_tls_config(&crt, default_tls_key(&crt).as_deref()).is_ok());

    // A cert file that also holds the key needs no --tls-key.
    let both = tmp.path().join("both.pem");
    let mut data = std::fs::read(&cert).unwrap();
    data.extend(std::fs::read(&key).unwrap());
    std::fs::write(&both, data).unwrap();
    assert_eq!(default_tls_key(&both), None);
    assert!(load_tls_config(&both, None).is_ok());

    let name = cert_hostname(&cert);
    assert_eq!(name.as_deref(), Some("mac.tail1.ts.net"));
    assert_eq!(
        phone_url("100.1.2.3", 8787, None, None, name.as_deref(), "http").as_deref(),
        Some("https://mac.tail1.ts.net:8787")
    );
    assert_eq!(
        phone_url("0.0.0.0", 443, None, None, name.as_deref(), "http").as_deref(),
        Some("https://mac.tail1.ts.net")
    );
    assert_eq!(
        phone_url("100.1.2.3", 8787, None, None, None, "https").as_deref(),
        Some("https://100.1.2.3:8787")
    );
}

#[test]
fn screenshots_go_out_as_lossless_webp() {
    use base64::Engine;
    let png = net::icon_png(16);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let (bytes, mime) = psycommux::panels::shot_image(&b64).unwrap();
    assert_eq!(mime, "image/webp");
    assert_eq!((&bytes[..4], &bytes[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));
    // Lossless: decoding gives back every pixel.
    let back = webp::Decoder::new(&bytes).decode().unwrap();
    let orig = image::load_from_memory(&png).unwrap().to_rgb8();
    assert_eq!((back.width(), back.height()), (16, 16));
    assert_eq!(&*back, orig.as_raw().as_slice());
    let (bytes, mime) = psycommux::panels::shot_image("iVBORw0KGgo=").unwrap();
    assert_eq!((mime, &bytes[..]), ("image/png", &b"\x89PNG\r\n\x1a\n"[..]));
    assert!(psycommux::panels::shot_image("not base64!").is_err());
}

#[test]
fn page_urls_fit_in_a_header() {
    use psycommux::panels::header_safe_url;
    assert_eq!(
        header_safe_url("https://a.com/x?q=1#f"),
        "https://a.com/x?q=1#f"
    );
    assert_eq!(
        header_safe_url("https://a.com/转录 x"),
        "https://a.com/%E8%BD%AC%E5%BD%95%20x"
    );
}

#[test]
fn push_encryption_matches_rfc_8291() {
    use base64::Engine;
    let b64 = |s: &str| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.replace(' ', ""))
            .unwrap()
    };
    // RFC 8291, section 5 and appendix A.
    let ua_public = b64(
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
    );
    let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
    let body = psycommux::push::encrypt(
        b"When I grow up, I want to be a watermelon",
        &ua_public,
        &b64("BTBZMqHH6r4Tts7J_aSIgg"),
        &b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"),
        &salt,
    )
    .unwrap();
    let expected = b64(
        "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml\
         mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT\
         pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
    );
    assert_eq!(body, expected);
}

#[test]
fn a_cmux_notification_as_a_push_message() {
    let m = psycommux::push::notification_message(&serde_json::json!({
        "id": "n1", "title": "Claude Code", "subtitle": "api", "body": "Waiting for input",
        "workspace_id": "W1", "surface_id": "S1",
    }));
    assert_eq!(m["title"], "Claude Code");
    assert_eq!(m["body"], "api\nWaiting for input");
    assert_eq!(m["tag"], "n1");
    assert_eq!(m["url"], "/?ws=W1&s=S1");
}

#[test]
fn faint_terminal_text_is_a_solid_color() {
    // iOS will not select text drawn with opacity, so faint becomes a blend.
    let grid = json!({
        "terminal_foreground": "#FFFFFF", "terminal_background": "#1E1E1E",
        "styles": [
            {"id": 4, "faint": true},
            {"id": 5, "faint": true, "foreground_source": "palette", "foreground": "#FF0000"},
            {"id": 6, "faint": true, "foreground_source": "palette", "foreground": "red"},
        ],
        "scrollback_rows": 0, "rows": 0,
    });
    let styles = &psycommux::cmux::compact_grid(&grid)["styles"];
    assert_eq!(styles["4"], "color:#A5A5A5");
    assert_eq!(styles["5"], "color:#A50C0C");
    assert_eq!(styles["6"], "color:red;opacity:.6");
    assert_eq!(
        psycommux::cmux::blend("#000000", "#FFFFFF", 0.5).as_deref(),
        Some("#808080")
    );
}

fn line(uuid: &str, parent: Option<&str>, role: &str, text: &str) -> Vec<u8> {
    jl(json!({"type": role, "uuid": uuid, "parentUuid": parent,
              "message": {"role": role, "content": [{"type": "text", "text": text}]}}))
}

fn texts(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["text"].as_str().unwrap_or("").to_string())
        .collect()
}

/// After a rewind the next prompt follows an earlier line; what came between is gone.
#[test]
fn rewound_turns_are_left_out() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("r.jsonl");
    let mut data = Vec::new();
    data.extend(line("u1", None, "user", "one"));
    data.extend(line("a1", Some("u1"), "assistant", "ONE"));
    data.extend(jl(
        json!({"type": "attachment", "uuid": "h1", "parentUuid": "a1", "attachment": {}}),
    ));
    data.extend(line("u2", Some("h1"), "user", "two"));
    data.extend(line("a2", Some("u2"), "assistant", "TWO"));
    data.extend(jl(json!({"type": "last-prompt", "leafUuid": "a2"})));
    let before_rewind = data.len() as u64;
    data.extend(line("u3", Some("h1"), "user", "three"));
    data.extend(line("a3", Some("u3"), "assistant", "THREE"));
    std::fs::write(&path, &data).unwrap();

    let page = read_tail(&path, None, 80, agentchat::CHUNK).unwrap();
    assert_eq!(texts(&page), ["one", "ONE", "three", "THREE"]);
    // Small chunks and pages: the rewind is followed across chunks, and the
    // earlier page starts before it.
    let last = read_tail(&path, None, 2, 60).unwrap();
    assert_eq!(texts(&last), ["three", "THREE"]);
    let earlier = read_tail(&path, last["start"].as_u64(), 80, 60).unwrap();
    assert_eq!(texts(&earlier), ["one", "ONE"]);

    // Read forward from before the rewind: the page shown is out of date.
    let update = read_since(&path, before_rewind).unwrap();
    assert_eq!(update["reset"], true);
    assert_eq!(texts(&update), ["one", "ONE", "three", "THREE"]);
}

#[test]
fn prompt_following_the_last_line_reads_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("f.jsonl");
    let mut data = line("u1", None, "user", "one");
    data.extend(line("a1", Some("u1"), "assistant", "ONE"));
    let offset = data.len() as u64;
    data.extend(jl(json!({"type": "mode", "mode": "normal"})));
    data.extend(line("u2", Some("a1"), "user", "two"));
    std::fs::write(&path, &data).unwrap();
    let update = read_since(&path, offset).unwrap();
    assert_eq!(update["reset"], Value::Null);
    assert_eq!(texts(&update), ["two"]);
}

/// A prompt whose earlier line is not in the file (say, a forked session) hides nothing.
#[test]
fn missing_parent_hides_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("m.jsonl");
    let mut data = line("u1", None, "user", "one");
    data.extend(line("a1", Some("u1"), "assistant", "ONE"));
    data.extend(line("u2", Some("elsewhere"), "user", "two"));
    std::fs::write(&path, &data).unwrap();
    let page = read_tail(&path, None, 1, 40).unwrap();
    assert_eq!(texts(&page), ["one", "ONE", "two"]);
}
