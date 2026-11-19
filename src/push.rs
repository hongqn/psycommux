//! Web Push to the phone's Home Screen app, so cmux notifications (and
//! `psycommux secret` requests) reach it while the app is closed.
//!
//! Messages are encrypted for the subscribing browser (RFC 8291, aes128gcm)
//! and signed with this server's VAPID key (RFC 8292): the push service
//! (Apple's, for an iPhone) carries them without being able to read them.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use bytes::Bytes;
use hkdf::Hkdf;
use http_body_util::Full;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::broadcast::error::RecvError;

use crate::cmux::value_text;
use crate::events::EventHub;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;
/// Away from the Mac: its screen is locked, or nobody touched it for this long.
const AWAY_IDLE: Duration = Duration::from_secs(120);
const MAX_BODY_CHARS: usize = 1000;
const RECORD_SIZE: u32 = 4096;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    /// The VAPID private key: 32 bytes, base64url.
    vapid_private: String,
    subscriptions: Vec<Subscription>,
}

type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub struct Push {
    /// Where keys and subscriptions are kept; None keeps them in memory.
    file: Option<PathBuf>,
    stored: Mutex<Stored>,
    /// VAPID `sub`: how the push service can reach whoever runs this server.
    contact: String,
    /// Push only while the Mac looks unattended (the phone is for when away).
    only_when_away: bool,
    http: HttpClient,
}

pub struct Sent {
    pub sent: usize,
    pub failed: Vec<String>,
}

impl Push {
    /// Keys and subscriptions in memory only, pushing whenever asked.
    pub fn memory() -> Arc<Self> {
        Arc::new(Self::build(
            None,
            new_secret(),
            Vec::new(),
            "mailto:psycommux@localhost",
        ))
    }

    /// Keep keys and subscriptions in `file` (created 0600), so phones stay
    /// subscribed across restarts.
    pub fn open(file: &Path, contact: &str, only_when_away: bool) -> std::io::Result<Arc<Self>> {
        let stored: Option<Stored> = match std::fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let stored = stored.filter(|s| {
            B64.decode(&s.vapid_private)
                .ok()
                .is_some_and(|k| SecretKey::from_slice(&k).is_ok())
        });
        let fresh = stored.is_none();
        let stored = stored.unwrap_or_else(|| Stored {
            vapid_private: B64.encode(new_secret()),
            subscriptions: Vec::new(),
        });
        let key = B64.decode(&stored.vapid_private).unwrap_or_default();
        let mut push = Self::build(Some(file.to_path_buf()), key, stored.subscriptions, contact);
        push.only_when_away = only_when_away;
        if fresh {
            push.save()?;
        }
        Ok(Arc::new(push))
    }

    fn build(
        file: Option<PathBuf>,
        key: Vec<u8>,
        subscriptions: Vec<Subscription>,
        contact: &str,
    ) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            file,
            stored: Mutex::new(Stored {
                vapid_private: B64.encode(key),
                subscriptions,
            }),
            contact: contact.to_string(),
            only_when_away: false,
            http: Client::builder(TokioExecutor::new()).build(https),
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let data = serde_json::to_vec_pretty(&*self.stored.lock().unwrap())?;
        let tmp = file.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&data)?;
        }
        std::fs::rename(&tmp, file)
    }

    fn signing_key(&self) -> SigningKey {
        let key = B64
            .decode(&self.stored.lock().unwrap().vapid_private)
            .unwrap_or_default();
        SigningKey::from_slice(&key).expect("valid VAPID key")
    }

    /// The VAPID public key a browser subscribes with (base64url, uncompressed).
    pub fn public_key(&self) -> String {
        let point = self.signing_key().verifying_key().to_encoded_point(false);
        B64.encode(point.as_bytes())
    }

    pub fn count(&self) -> usize {
        self.stored.lock().unwrap().subscriptions.len()
    }

    /// Add (or refresh) a browser's subscription, as `PushSubscription.toJSON()` gives it.
    pub fn subscribe(&self, sub: &Value) -> Result<(), String> {
        let text = |ptr: &str| sub.pointer(ptr).map(value_text).unwrap_or_default();
        let sub = Subscription {
            endpoint: text("/endpoint"),
            p256dh: text("/keys/p256dh").trim_end_matches('=').to_string(),
            auth: text("/keys/auth").trim_end_matches('=').to_string(),
        };
        let url = url::Url::parse(&sub.endpoint).map_err(|_| "bad endpoint".to_string())?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err("the endpoint must be https".into());
        }
        let p256dh = B64.decode(&sub.p256dh).map_err(|_| "bad p256dh")?;
        PublicKey::from_sec1_bytes(&p256dh).map_err(|_| "bad p256dh")?;
        if B64.decode(&sub.auth).map(|a| a.len()) != Ok(16) {
            return Err("bad auth".into());
        }
        {
            let mut stored = self.stored.lock().unwrap();
            stored.subscriptions.retain(|s| s.endpoint != sub.endpoint);
            stored.subscriptions.push(sub);
        }
        self.save().map_err(|e| e.to_string())
    }

    pub fn unsubscribe(&self, endpoint: &str) -> bool {
        let removed = {
            let mut stored = self.stored.lock().unwrap();
            let before = stored.subscriptions.len();
            stored.subscriptions.retain(|s| s.endpoint != endpoint);
            before != stored.subscriptions.len()
        };
        if removed {
            let _ = self.save();
        }
        removed
    }

    /// Send `message` ({title, body, tag, url}) to every subscription; drop
    /// those the push service says are gone.
    pub async fn send_all(&self, message: &Value) -> Sent {
        let subs = self.stored.lock().unwrap().subscriptions.clone();
        let mut sent = Sent {
            sent: 0,
            failed: Vec::new(),
        };
        let payload = message.to_string();
        for sub in subs {
            match self.send(&sub, payload.as_bytes()).await {
                Ok(status) if (200..300).contains(&status) => sent.sent += 1,
                Ok(status @ (404 | 410)) => {
                    self.unsubscribe(&sub.endpoint);
                    sent.failed
                        .push(format!("{status}: subscription expired, removed"));
                }
                Ok(status) => sent.failed.push(format!("push service answered {status}")),
                Err(e) => sent.failed.push(e),
            }
        }
        sent
    }

    async fn send(&self, sub: &Subscription, payload: &[u8]) -> Result<u16, String> {
        let p256dh = B64.decode(&sub.p256dh).map_err(|e| e.to_string())?;
        let auth = B64.decode(&sub.auth).map_err(|e| e.to_string())?;
        let mut salt = [0u8; 16];
        crate::net::getrandom(&mut salt);
        let body = encrypt(payload, &p256dh, &auth, &new_secret(), &salt)?;
        let request = hyper::Request::post(&sub.endpoint)
            .header("ttl", "86400")
            .header("urgency", "high")
            .header("content-encoding", "aes128gcm")
            .header("content-type", "application/octet-stream")
            .header("authorization", self.vapid(&sub.endpoint)?)
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(Duration::from_secs(15), self.http.request(request))
            .await
            .map_err(|_| "push service timed out".to_string())?
            .map_err(|e| format!("push service unreachable: {e}"))?;
        Ok(response.status().as_u16())
    }

    /// `Authorization: vapid t=<JWT>, k=<public key>` for this endpoint's origin.
    fn vapid(&self, endpoint: &str) -> Result<String, String> {
        let url = url::Url::parse(endpoint).map_err(|e| e.to_string())?;
        let audience = url.origin().ascii_serialization();
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 12 * 3600;
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims =
            B64.encode(json!({"aud": audience, "exp": exp, "sub": self.contact}).to_string());
        let signed = format!("{header}.{claims}");
        let signature: Signature = self.signing_key().sign(signed.as_bytes());
        Ok(format!(
            "vapid t={signed}.{}, k={}",
            B64.encode(signature.to_bytes()),
            self.public_key()
        ))
    }

    /// Push each new cmux notification while the Mac is unattended.
    pub fn start(self: &Arc<Self>, hub: &EventHub) {
        let this = Arc::clone(self);
        let mut rx = hub.subscribe();
        tokio::spawn(async move {
            loop {
                let message = match rx.recv().await {
                    Ok(m) => m,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                };
                if message.get("name").and_then(Value::as_str) != Some("notification.created") {
                    continue;
                }
                let Some(n) = message.get("notification").filter(|n| n.is_object()) else {
                    continue;
                };
                if this.count() == 0 || (this.only_when_away && !mac_unattended().await) {
                    continue;
                }
                let sent = this.send_all(&notification_message(n)).await;
                for failure in sent.failed {
                    eprintln!("push: {failure}");
                }
            }
        });
    }
}

/// What a cmux notification becomes on the phone.
pub fn notification_message(n: &Value) -> Value {
    let text = |key: &str| n.get(key).map(value_text).unwrap_or_default();
    let body: String = [text("subtitle"), text("body")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .take(MAX_BODY_CHARS)
        .collect();
    let mut url = url::Url::parse("http://x/").unwrap();
    for (key, name) in [("workspace_id", "ws"), ("surface_id", "s")] {
        let v = text(key);
        if !v.is_empty() {
            url.query_pairs_mut().append_pair(name, &v);
        }
    }
    let title = Some(text("title")).filter(|t| !t.is_empty());
    json!({
        "title": title.unwrap_or_else(|| "psycommux".into()),
        "body": body,
        "tag": text("id"),
        "url": format!("/{}", url.query().map(|q| format!("?{q}")).unwrap_or_default()),
    })
}

/// The Mac's screen is locked, or its keyboard and mouse sat idle a while.
pub async fn mac_unattended() -> bool {
    let run = |args: &'static [&'static str]| async move {
        tokio::process::Command::new("/usr/sbin/ioreg")
            .args(args)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let root = run(&["-n", "Root", "-d1", "-a"]).await;
    if let Some(at) = root.find("<key>CGSSessionScreenIsLocked</key>")
        && root[at..]
            .trim_start_matches("<key>CGSSessionScreenIsLocked</key>")
            .trim_start()
            .starts_with("<true/>")
    {
        return true;
    }
    let hid = run(&["-c", "IOHIDSystem", "-d", "4"]).await;
    hid.lines()
        .find(|l| l.contains("\"HIDIdleTime\""))
        .and_then(|l| l.rsplit('=').next())
        .and_then(|n| n.trim().parse::<u64>().ok())
        .is_some_and(|ns| Duration::from_nanos(ns) >= AWAY_IDLE)
}

fn new_secret() -> Vec<u8> {
    loop {
        let mut bytes = [0u8; 32];
        crate::net::getrandom(&mut bytes);
        if SecretKey::from_slice(&bytes).is_ok() {
            return bytes.to_vec();
        }
    }
}

/// RFC 8291 aes128gcm: one record holding `plaintext`, for the browser key
/// `ua_public` (uncompressed point) and `auth`, from the one-off key
/// `as_secret` and `salt`.
pub fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    as_secret: &[u8],
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    if plaintext.len() + 1 + 16 > RECORD_SIZE as usize - 86 {
        return Err("push message too long".into());
    }
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "bad browser key")?;
    let as_key = SecretKey::from_slice(as_secret).map_err(|_| "bad server key")?;
    let as_public = as_key.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(as_key.to_nonzero_scalar(), ua.as_affine());

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| e.to_string())?;
    let content = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    content
        .expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .and_then(|()| content.expand(b"Content-Encoding: nonce\0", &mut nonce))
        .map_err(|e| e.to_string())?;

    let mut record = plaintext.to_vec();
    record.push(2); // last record, no padding
    let sealed = Aes128Gcm::new_from_slice(&cek)
        .map_err(|e| e.to_string())?
        .encrypt(&Nonce::from(nonce), record.as_ref())
        .map_err(|e| e.to_string())?;

    let mut out = salt.to_vec();
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&sealed);
    Ok(out)
}
