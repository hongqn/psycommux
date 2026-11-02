//! Browser, Markdown and file-preview surfaces.
//!
//! Markdown and file-preview panels: the socket API does not report which
//! file a panel shows, but cmux's saved session (Application Support/cmux/
//! session-<bundle id>.json, rewritten as panels change) records `filePath`
//! for each one. That file is cmux's own format and may change; when it does,
//! these panels fall back to "not available".
//!
//! Browser panels: `browser.screenshot` returns a PNG of the page; taps,
//! scrolls and typing run as small scripts through `browser.eval`, because the
//! socket has no raw pointer input (`browser.input_mouse` is unsupported).

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

pub const MAX_FILE_BYTES: usize = 2 << 20;

pub fn default_session_glob() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/Library/Application Support/cmux/session-com.cmuxterm.app*.json")
}

/// surface id -> file path for Markdown and file-preview panels, from cmux's saved session.
pub struct SessionFiles {
    pattern: String,
    cache: Mutex<(Option<(PathBuf, SystemTime)>, HashMap<String, String>)>,
}

impl SessionFiles {
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            cache: Mutex::new((None, HashMap::new())),
        }
    }

    /// The running app's file, not the "-previous" backup or another channel's.
    fn session_file(&self) -> Option<(PathBuf, SystemTime)> {
        glob::glob(&self.pattern)
            .ok()?
            .filter_map(Result::ok)
            .filter(|p| !p.to_string_lossy().ends_with("-previous.json"))
            .filter_map(|p| {
                let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                Some((p, mtime))
            })
            .max_by_key(|(_, mtime)| *mtime)
    }

    pub fn path_for(&self, surface_id: &str) -> Option<String> {
        let stamp = self.session_file()?;
        let mut cache = self.cache.lock().unwrap();
        if cache.0.as_ref() != Some(&stamp) {
            let data = std::fs::read(&stamp.0)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let mut paths = HashMap::new();
            if let Some(data) = &data {
                walk(data, &mut paths);
            }
            *cache = (Some(stamp), paths);
        }
        cache.1.get(&surface_id.to_uppercase()).cloned()
    }
}

fn walk(node: &Value, paths: &mut HashMap<String, String>) {
    match node {
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str);
            if let (Some("markdown" | "filepreview"), Some(id)) =
                (kind, map.get("id").and_then(Value::as_str))
            {
                let inner = map
                    .get("markdown")
                    .filter(|v| !v.is_null())
                    .or_else(|| map.get("filePreview"))
                    .and_then(|v| v.get("filePath"))
                    .and_then(Value::as_str);
                if let Some(path) = inner {
                    paths.insert(id.to_uppercase(), path.to_string());
                }
            }
            for value in map.values() {
                walk(value, paths);
            }
        }
        Value::Array(items) => items.iter().for_each(|v| walk(v, paths)),
        _ => {}
    }
}

/// The file's text (UTF-8, replaced errors) with a stamp; `unchanged` when it matches `known_stamp`.
pub fn read_file(path: &Path, known_stamp: Option<&str>) -> std::io::Result<Value> {
    let meta = std::fs::metadata(path)?;
    let mtime_ns = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stamp = format!("{mtime_ns}:{}", meta.len());
    if known_stamp == Some(stamp.as_str()) {
        return Ok(json!({"unchanged": true, "stamp": stamp}));
    }
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut data)?;
    let truncated = data.len() > MAX_FILE_BYTES;
    data.truncate(MAX_FILE_BYTES);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(json!({
        "text": String::from_utf8_lossy(&data),
        "stamp": stamp,
        "truncated": truncated,
        "name": name,
        "path": path.to_string_lossy(),
    }))
}

pub fn shot_hash(png_base64: &str) -> String {
    crate::cmux::short_hash(png_base64.as_bytes())
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// Tap at (fx, fy): fractions 0..1 across the visible viewport (the screenshot).
pub fn tap_script(fx: f64, fy: f64) -> String {
    let (fx, fy) = (clamp(fx, 0.0, 1.0), clamp(fy, 0.0, 1.0));
    format!(
        r#"(() => {{
  const x = {fx:.6} * innerWidth, y = {fy:.6} * innerHeight;
  const el = document.elementFromPoint(x, y);
  if (!el) return null;
  const opts = {{ bubbles: true, cancelable: true, composed: true, clientX: x, clientY: y, view: window, button: 0 }};
  el.dispatchEvent(new PointerEvent('pointerdown', opts));
  el.dispatchEvent(new MouseEvent('mousedown', opts));
  if (typeof el.focus === 'function') el.focus({{ preventScroll: true }});
  el.dispatchEvent(new PointerEvent('pointerup', opts));
  el.dispatchEvent(new MouseEvent('mouseup', opts));
  el.click();
  return el.tagName.toLowerCase();
}})()"#
    )
}

/// Scroll the nearest scrollable ancestor of the point, or the page.
pub fn scroll_script(fx: f64, fy: f64, dx: f64, dy: f64) -> String {
    let (fx, fy) = (clamp(fx, 0.0, 1.0), clamp(fy, 0.0, 1.0));
    let (dx, dy) = (clamp(dx, -5.0, 5.0), clamp(dy, -5.0, 5.0));
    format!(
        r#"(() => {{
  const x = {fx:.6} * innerWidth, y = {fy:.6} * innerHeight;
  const dy = {dy:.6} * innerHeight, dx = {dx:.6} * innerWidth;
  let el = document.elementFromPoint(x, y);
  while (el && el !== document.body && el !== document.documentElement) {{
    const s = getComputedStyle(el);
    const canY = dy && /(auto|scroll|overlay)/.test(s.overflowY) && el.scrollHeight > el.clientHeight;
    const canX = dx && /(auto|scroll|overlay)/.test(s.overflowX) && el.scrollWidth > el.clientWidth;
    if (canY || canX) {{ el.scrollBy(dx, dy); return 'element'; }}
    el = el.parentElement;
  }}
  window.scrollBy(dx, dy);
  return 'page';
}})()"#
    )
}

/// Insert text where the page's focus is, as typing would (fires input events).
pub fn type_script(text: &str) -> String {
    let literal = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(() => {{
  const el = document.activeElement;
  if (!el || el === document.body) return false;
  return document.execCommand('insertText', false, {literal});
}})()"#
    )
}
