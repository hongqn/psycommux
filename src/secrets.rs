//! Secrets handed from the phone to an agent.
//!
//! `psycommux secret <label>` asks over a Unix socket that only this user can
//! open. The request shows on the phone (and as a cmux notification); the
//! value typed or autofilled there goes back over that socket, and psycommux
//! never writes it to disk or to its log.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::cmux::{value_text, CmuxClient};
use crate::events::EventHub;

const MAX_LABEL_CHARS: usize = 200;
const MAX_REQUEST_BYTES: u64 = 4096;
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// Where the server listens for `psycommux secret`: under the home directory,
/// not $TMPDIR, which agents' sandboxes often point elsewhere.
pub fn socket_path(port: u16) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".psycommux").join(format!("secret-{port}.sock"))
}

enum Answer {
    Value(String),
    Declined,
}

struct Pending {
    label: String,
    workspace_id: String,
    surface_id: String,
    requested_at: u64,
    reply: oneshot::Sender<Answer>,
}

pub struct SecretRequests {
    pending: Mutex<HashMap<String, Pending>>,
    client: Arc<CmuxClient>,
    hub: Arc<EventHub>,
}

impl SecretRequests {
    pub fn new(client: Arc<CmuxClient>, hub: Arc<EventHub>) -> Arc<Self> {
        Arc::new(Self {
            pending: Mutex::new(HashMap::new()),
            client,
            hub,
        })
    }

    /// The open requests, oldest first; labels only, never values.
    pub fn list(&self) -> Value {
        let pending = self.pending.lock().unwrap();
        let mut rows: Vec<_> = pending.iter().collect();
        rows.sort_by_key(|(_, p)| p.requested_at);
        Value::Array(
            rows.into_iter()
                .map(|(id, p)| {
                    json!({"id": id, "label": p.label, "workspace_id": p.workspace_id,
                           "surface_id": p.surface_id, "requested_at": p.requested_at})
                })
                .collect(),
        )
    }

    /// Hand `value` to the waiting command, or decline with `None`. False when
    /// no such request is open (answered already, or the command gave up).
    pub fn answer(&self, id: &str, value: Option<String>) -> bool {
        let Some(pending) = self.pending.lock().unwrap().remove(id) else {
            return false;
        };
        let _ = pending.reply.send(match value {
            Some(v) => Answer::Value(v),
            None => Answer::Declined,
        });
        self.changed();
        true
    }

    fn changed(&self) {
        self.hub
            .publish(json!({"type": "secrets", "requests": self.list()}));
    }

    /// Listen at `path`: a 0700 directory and a 0600 socket.
    pub fn listen(self: &Arc<Self>, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another psycommux answers at {}", path.display()),
            ));
        }
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(Arc::clone(&this).serve(stream));
            }
        });
        Ok(())
    }

    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        if (&mut reader)
            .take(MAX_REQUEST_BYTES)
            .read_line(&mut line)
            .await
            .is_err()
        {
            return;
        }
        let request: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
        let text = |key: &str| request.get(key).map(value_text).unwrap_or_default();
        let label: String = text("label").trim().chars().take(MAX_LABEL_CHARS).collect();
        if label.is_empty() {
            let _ = write
                .write_all(b"{\"ok\":false,\"error\":\"a label is required\"}\n")
                .await;
            return;
        }
        let id = crate::net::new_token()[..12].to_string();
        let (tx, rx) = oneshot::channel();
        let pending = Pending {
            label: label.clone(),
            workspace_id: text("workspace_id"),
            surface_id: text("surface_id"),
            requested_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            reply: tx,
        };
        let mut params = json!({"title": "Secret requested", "subtitle": label,
                                "body": format!("{label}: provide it in psycommux on your phone")});
        for key in ["workspace_id", "surface_id"] {
            let v = text(key);
            if !v.is_empty() {
                params[key] = json!(v);
            }
        }
        self.pending.lock().unwrap().insert(id.clone(), pending);
        self.changed();
        // cmux answers notification.create without the new id; find it again
        // by its text when the request ends.
        let body = params["body"].clone();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.client.call("notification.create", params),
        )
        .await;

        // Wait for the phone, or for the command to give up (EOF).
        let gone = async {
            let mut buf = [0u8; 256];
            while matches!(reader.read(&mut buf).await, Ok(n) if n > 0) {}
        };
        let outcome = tokio::select! {
            answer = rx => answer.ok(),
            () = gone => None,
        };
        if self.pending.lock().unwrap().remove(&id).is_some() {
            self.changed();
        }
        self.dismiss_notification(&label, &body).await;
        let reply = match outcome {
            Some(Answer::Value(v)) => json!({"ok": true, "value": v}),
            Some(Answer::Declined) => json!({"ok": false, "error": "declined on the phone"}),
            None => return,
        };
        let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    }
}

impl SecretRequests {
    async fn dismiss_notification(&self, label: &str, body: &Value) {
        let Ok(list) = self.client.call("notification.list", json!({})).await else {
            return;
        };
        let items = list.get("notifications").and_then(Value::as_array);
        for n in items.into_iter().flatten() {
            if n.get("title").map(value_text).as_deref() == Some("Secret requested")
                && n.get("subtitle").map(value_text).as_deref() == Some(label)
                && n.get("body") == Some(body)
            {
                if let Some(id) = n.get("id") {
                    let _ = self
                        .client
                        .call("notification.dismiss", json!({"id": id}))
                        .await;
                }
            }
        }
    }
}

/// The client side of `psycommux secret`: ask, and wait up to `timeout`.
pub async fn request(
    path: &Path,
    label: &str,
    workspace_id: &str,
    surface_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut stream = UnixStream::connect(path).await.map_err(|e| {
        format!(
            "cannot reach psycommux at {} ({e}); is it running, on this --port?",
            path.display()
        )
    })?;
    let ask = json!({"label": label, "workspace_id": workspace_id, "surface_id": surface_id});
    stream
        .write_all(format!("{ask}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    let mut reader = BufReader::new(&mut stream).take(MAX_SECRET_BYTES as u64 * 2 + 1024);
    let read = reader.read_line(&mut line);
    match tokio::time::timeout(timeout, read).await {
        Err(_) => return Err(format!("no answer within {} s", timeout.as_secs())),
        Ok(Err(e)) => return Err(e.to_string()),
        Ok(Ok(0)) => return Err("psycommux closed the request".into()),
        Ok(Ok(_)) => {}
    }
    let reply: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    if reply.get("ok") == Some(&Value::Bool(true)) {
        Ok(reply.get("value").map(value_text).unwrap_or_default())
    } else {
        Err(reply
            .get("error")
            .map(value_text)
            .unwrap_or_else(|| "refused".into()))
    }
}
