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

/// A URL as an HTTP header value: non-ASCII and control bytes percent-encoded.
pub fn header_safe_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for b in url.bytes() {
        if (0x21..0x7f).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A screenshot as sent to the viewer: the image bytes and their MIME type.
///
/// cmux sends PNG; lossless WebP keeps every pixel (text in a 1x screenshot
/// does not survive lossy compression well) at about a third of the size. A
/// screenshot that does not decode goes out as the PNG it is.
pub fn shot_image(png_base64: &str) -> Result<(Vec<u8>, &'static str), String> {
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(png_base64.trim())
        .map_err(|e| format!("screenshot is not base64: {e}"))?;
    let Ok(decoded) = image::load_from_memory_with_format(&png, image::ImageFormat::Png) else {
        return Ok((png, "image/png"));
    };
    let rgb = decoded.to_rgb8();
    let webp = webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height()).encode_lossless();
    Ok((webp.to_vec(), "image/webp"))
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

/// Scroll the nearest scrollable ancestor of the point, or the page, by
/// fractions of the viewport. Returns how far it moved, in the same fractions:
/// less than asked at an edge, so the viewer can settle its preview.
pub fn scroll_script(fx: f64, fy: f64, dx: f64, dy: f64) -> String {
    let (fx, fy) = (clamp(fx, 0.0, 1.0), clamp(fy, 0.0, 1.0));
    let (dx, dy) = (clamp(dx, -5.0, 5.0), clamp(dy, -5.0, 5.0));
    format!(
        r#"(() => {{
  const x = {fx:.6} * innerWidth, y = {fy:.6} * innerHeight;
  const dy = {dy:.6} * innerHeight, dx = {dx:.6} * innerWidth;
  const moved = (target, at) => {{
    const before = at();
    target.scrollBy({{ left: dx, top: dy, behavior: 'instant' }});
    const after = at();
    return {{ dx: (after[0] - before[0]) / innerWidth, dy: (after[1] - before[1]) / innerHeight }};
  }};
  let el = document.elementFromPoint(x, y);
  while (el && el !== document.body && el !== document.documentElement) {{
    const s = getComputedStyle(el);
    const canY = dy && /(auto|scroll|overlay)/.test(s.overflowY) && el.scrollHeight > el.clientHeight;
    const canX = dx && /(auto|scroll|overlay)/.test(s.overflowX) && el.scrollWidth > el.clientWidth;
    if (canY || canX) return moved(el, () => [el.scrollLeft, el.scrollTop]);
    el = el.parentElement;
  }}
  return moved(window, () => [scrollX, scrollY]);
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

/// Reader mode: the page's main content as a small tree, for the phone to lay
/// out as its own text. A node is a string (text) or `[tag, attrs, ...children]`
/// with tags from a short list and attrs only `href` (http/https) or `src`/`alt`.
/// The phone builds elements from it and never parses the page's HTML, so
/// nothing from the page runs on psycommux's origin. Reads computed styles,
/// not layout, so it works on a panel the Mac is not showing.
pub const READ_SCRIPT: &str = r#"(() => {
  const SKIP = new Set(['SCRIPT','STYLE','NOSCRIPT','TEMPLATE','SVG','CANVAS','IFRAME','NAV','ASIDE','FOOTER','BUTTON','INPUT','SELECT','TEXTAREA','FORM','DIALOG','OBJECT','EMBED','VIDEO','AUDIO','MAP','HEAD','LINK','META']);
  const KEEP = new Set(['H1','H2','H3','H4','H5','H6','P','UL','OL','LI','BLOCKQUOTE','A','STRONG','EM','B','I','U','BR','HR','IMG','TABLE','THEAD','TBODY','TFOOT','TR','TH','TD','FIGURE','FIGCAPTION','DEL','S','SUP','SUB','DL','DT','DD','KBD','MARK','CODE','PRE','CAPTION','SMALL']);
  const BLOCK = new Set(['div','p','h1','h2','h3','h4','h5','h6','ul','ol','li','blockquote','pre','table','thead','tbody','tfoot','tr','th','td','figure','figcaption','dl','dt','dd','hr','img','caption']);
  const web = (u) => /^https?:/i.test(u || '') ? u : null;
  const hidden = (el) => {
    if (el.hidden || el.getAttribute('aria-hidden') === 'true') return true;
    const s = getComputedStyle(el);
    if (s.display === 'none' || s.visibility === 'hidden' || s.visibility === 'collapse') return true;
    // Screen-reader-only text, clipped to nothing.
    return s.position === 'absolute' && (/rect\(0(px)?,? 0(px)?,? 0(px)?,? 0(px)?\)/.test(s.clip) || s.clipPath === 'inset(50%)' || s.width === '1px');
  };
  // Visible text when the panel has a layout (the viewport lease gives it one); hidden text such
  // as GitHub's raw-file textarea would otherwise outweigh the article.
  const words = (el) => ((innerWidth ? el.innerText : el.textContent) || '').replace(/\s+/g, ' ').trim().length;
  // The main content: the tightest semantic container holding at least half the biggest one's
  // text (GitHub's README article inside its main), else where the paragraphs gather.
  const pick = () => {
    const found = [...document.querySelectorAll('article, main, [role=main], [itemprop=articleBody]')]
      .filter((el) => !hidden(el)).map((el) => [el, words(el)]).sort((a, b) => b[1] - a[1]);
    if (found.length && found[0][1] > 400) {
      let [best, len] = found[0];
      for (const [el, n] of found) if (el !== best && best.contains(el) && n >= len / 2 && n < len) { best = el; len = n; }
      return best;
    }
    const score = new Map();
    for (const p of document.querySelectorAll('p, pre, li, blockquote, td')) {
      const n = words(p);
      if (n < 25) continue;
      const a = p.parentElement, b = a && a.parentElement;
      if (a) score.set(a, (score.get(a) || 0) + n);
      if (b) score.set(b, (score.get(b) || 0) + n / 2);
    }
    let top = null, topScore = 0;
    for (const [el, n] of score) if (n > topScore) { top = el; topScore = n; }
    if (top && /^(TR|TBODY|THEAD|TFOOT|TABLE)$/.test(top.tagName)) top = top.closest('table').parentElement || top;
    return top && topScore > 400 ? top : document.body;
  };
  // Tables used for layout (no headers, or tables inside) read as rows of text, not a grid.
  const layout = new WeakMap();
  const isLayout = (el) => {
    const t = el.closest('table');
    if (!t) return false;
    if (!layout.has(t)) layout.set(t, !t.querySelector('th, caption') || !!t.querySelector('table'));
    return layout.get(t);
  };
  const isBlock = (k) => Array.isArray(k) && BLOCK.has(k[0]);
  // Drop whitespace that only separates boxes; keep it between words.
  const tidy = (raw) => {
    const kids = [];
    for (const k of raw) {
      if (typeof k === 'string' && typeof kids[kids.length - 1] === 'string') kids[kids.length - 1] = (kids[kids.length - 1] + k).replace(/\s+/g, ' ');
      else kids.push(k);
    }
    return kids.filter((k, i) => typeof k !== 'string' || k.trim() ||
      (i > 0 && i < kids.length - 1 && !isBlock(kids[i - 1]) && !isBlock(kids[i + 1])));
  };
  let budget = 6000, chars = 400000;
  const walk = (node) => {
    if (budget-- <= 0 || chars <= 0) return null;
    if (node.nodeType === 3) {
      const t = node.nodeValue.replace(/\s+/g, ' ');
      if (!t) return null;
      chars -= t.length;
      return t;
    }
    if (node.nodeType !== 1) return null;
    const el = node, tag = el.tagName.toUpperCase();
    if (SKIP.has(tag) || hidden(el)) return null;
    if (tag === 'PRE') { const t = el.textContent; chars -= t.length; return ['pre', null, t]; }
    if (tag === 'IMG') {
      const src = web(el.currentSrc || el.src || el.getAttribute('data-src'));
      if (!src) return el.alt ? el.alt : null;
      // The width the page shows it at, so avatars and icons stay small on the phone.
      const w = Math.round(el.getBoundingClientRect().width) || el.width || 0;
      return ['img', { src, alt: el.alt || '', w }];
    }
    if (tag === 'BR' || tag === 'HR') return [tag.toLowerCase(), null];
    let kids = [];
    for (const c of el.childNodes) { const k = walk(c); if (k !== null) kids.push(k); }
    kids = tidy(kids);
    if (!kids.length || (kids.length === 1 && kids[0] === ' ')) return null;
    if (tag === 'A') {
      const href = web(el.href);
      return href ? ['a', { href }, ...kids] : ['span', null, ...kids];
    }
    if (/^(TABLE|THEAD|TBODY|TFOOT|TR|TD|TH)$/.test(tag) && isLayout(el)) {
      return tag === 'TD' || tag === 'TH' ? ['span', null, ...kids, ' '] : ['div', null, ...kids];
    }
    if (KEEP.has(tag)) return [tag.toLowerCase(), null, ...kids];
    const d = getComputedStyle(el).display;
    if (/^(inline|contents)$/.test(d)) return ['span', null, ...kids];
    // A box holding only one box adds nothing.
    if (kids.length === 1 && isBlock(kids[0])) return kids[0];
    return ['div', null, ...kids];
  };
  const root = pick();
  return { url: location.href, title: document.title, body: walk(root) };
})()"#;
