//! Read Claude Code transcripts as chat items for the chat view.
//!
//! cmux knows which terminal runs which agent session and where its
//! transcript lives (`chat.sessions.dump`). The transcript is Claude Code's
//! JSONL log; this module turns its tail into small display items, following
//! the rules of cmux's own ClaudeTranscriptParser.
//!
//! Transcripts grow to hundreds of megabytes, so pages are read backwards from
//! the end and live updates are read forward from a byte offset.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

use regex::bytes::Regex as BytesRegex;
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};

const NOISE_PREFIXES: [&str; 3] = ["<command-name>", "<local-command", "<system-reminder"];
const SUMMARY_KEYS: [&str; 11] = [
    "file_path",
    "notebook_path",
    "path",
    "pattern",
    "command",
    "query",
    "url",
    "description",
    "prompt",
    "skill",
    "name",
];
const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "NotebookEdit"];
const TEXT_LIMIT: usize = 20_000;
const DETAIL_LIMIT: usize = 4_000;
const RESULT_LIMIT: usize = 4_000;
const SUMMARY_LIMIT: usize = 160;
pub const CHUNK: u64 = 1 << 20;
const MAX_BACK_SCAN: u64 = 48 << 20;
const MAX_FORWARD_READ: u64 = 16 << 20;

/// Bookkeeping lines (api-request blobs run to megabytes) start with their
/// type, so they are skipped without parsing; chat lines start with other keys.
static LEADING_TYPE: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r#"^\{"type":"([a-z_-]+)""#).unwrap());
static COMMAND_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-name>(.*?)</command-name>").unwrap());
static COMMAND_ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<command-args>(.*?)</command-args>").unwrap());
static SUMMARY_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<summary>(.*?)</summary>").unwrap());

type Item = Map<String, Value>;

fn cut(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}\n… ({} more characters)", count - limit)
}

fn one_line(text: &str, limit: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= limit {
        return joined;
    }
    let head: String = joined.chars().take(limit - 1).collect();
    format!("{head}…")
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A unified diff body (no file headers) with 2 lines of context, and its +/- counts.
fn diff(old: &str, new: &str) -> (String, usize, usize) {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let text_diff = similar::TextDiff::from_slices(&old_lines, &new_lines);
    let mut out: Vec<String> = Vec::new();
    for hunk in text_diff.unified_diff().context_radius(2).iter_hunks() {
        out.push(hunk.header().to_string());
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Insert => '+',
                similar::ChangeTag::Equal => ' ',
            };
            out.push(format!("{sign}{}", change.value()));
        }
    }
    let adds = out.iter().filter(|l| l.starts_with('+')).count();
    let dels = out.iter().filter(|l| l.starts_with('-')).count();
    (out.join("\n"), adds, dels)
}

fn with(base: &Item, extra: Value) -> Item {
    let mut item = base.clone();
    if let Value::Object(extra) = extra {
        item.extend(extra);
    }
    item
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn nonempty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    s(v, key).filter(|x| !x.is_empty())
}

fn pretty(v: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    v.serialize(&mut ser).ok();
    String::from_utf8(buf).unwrap_or_default()
}

fn tool_items(block: &Value, base: &Item) -> Vec<Item> {
    let name = nonempty(block, "name").unwrap_or("tool");
    let call_id = s(block, "id").unwrap_or("");
    let empty = json!({});
    let inp = block
        .get("input")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    let id = if call_id.is_empty() {
        base.get("id").cloned().unwrap_or(json!(""))
    } else {
        json!(call_id)
    };
    let mut item = with(base, json!({"kind": "tool", "id": id, "tool": name}));

    if name == "Bash" {
        if let Some(command) = s(inp, "command") {
            item.insert("summary".into(), json!(one_line(command, SUMMARY_LIMIT)));
            item.insert("command".into(), json!(cut(command, DETAIL_LIMIT)));
            if let Some(desc) = inp
                .get("description")
                .filter(|d| !d.is_null() && d.as_str() != Some(""))
            {
                let desc = desc
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| desc.to_string());
                item.insert("note".into(), json!(one_line(&desc, SUMMARY_LIMIT)));
            }
            return vec![item];
        }
    }
    if name == "Write" || EDIT_TOOLS.contains(&name) {
        if let Some(path) = nonempty(inp, "file_path").or_else(|| nonempty(inp, "notebook_path")) {
            let (text, adds, dels) = if name == "Write" {
                diff("", s(inp, "content").unwrap_or(""))
            } else if name == "MultiEdit" {
                let parts: Vec<(String, usize, usize)> = inp
                    .get("edits")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|e| {
                        diff(
                            s(e, "old_string").unwrap_or(""),
                            s(e, "new_string").unwrap_or(""),
                        )
                    })
                    .collect();
                let text = parts
                    .iter()
                    .map(|p| p.0.as_str())
                    .collect::<Vec<_>>()
                    .join("\n…\n");
                (
                    text,
                    parts.iter().map(|p| p.1).sum(),
                    parts.iter().map(|p| p.2).sum(),
                )
            } else {
                let new = nonempty(inp, "new_string")
                    .or_else(|| nonempty(inp, "new_source"))
                    .unwrap_or("");
                diff(s(inp, "old_string").unwrap_or(""), new)
            };
            item.extend(
                json!({"summary": path, "path": path, "diff": cut(&text, DETAIL_LIMIT), "additions": adds, "deletions": dels})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            return vec![item];
        }
    }
    if name == "AskUserQuestion" {
        if let Some(questions) = inp.get("questions").and_then(Value::as_array) {
            let mut out = Vec::new();
            for (i, q) in questions.iter().enumerate() {
                let Some(prompt) = q.as_object().and(nonempty(q, "question")) else {
                    continue;
                };
                let options: Vec<Value> = q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|o| o.is_object())
                    .map(|o| json!({"label": s(o, "label").unwrap_or(""), "detail": s(o, "description").unwrap_or("")}))
                    .collect();
                out.push(with(
                    base,
                    json!({
                        "kind": "question", "id": format!("{call_id}#{i}"), "call_id": call_id,
                        "prompt": prompt, "header": s(q, "header").unwrap_or(""),
                        "multi": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        "options": options,
                    }),
                ));
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    if name == "TodoWrite" {
        if let Some(todos) = inp.get("todos").and_then(Value::as_array) {
            let todos: Vec<Value> = todos
                .iter()
                .filter(|t| t.is_object())
                .map(|t| {
                    let text = nonempty(t, "content")
                        .or_else(|| nonempty(t, "activeForm"))
                        .unwrap_or("");
                    json!({"text": text, "status": s(t, "status").unwrap_or("")})
                })
                .collect();
            let done = todos.iter().filter(|t| t["status"] == "completed").count();
            item.insert(
                "summary".into(),
                json!(format!("{done}/{} done", todos.len())),
            );
            item.insert("todos".into(), json!(todos));
            return vec![item];
        }
    }
    let summary = SUMMARY_KEYS
        .iter()
        .find_map(|key| s(inp, key).filter(|v| !v.trim().is_empty()))
        .map(|v| one_line(v, SUMMARY_LIMIT))
        .unwrap_or_default();
    let detail = if inp.as_object().is_some_and(|o| !o.is_empty()) {
        cut(&pretty(inp), DETAIL_LIMIT)
    } else {
        String::new()
    };
    item.insert("summary".into(), json!(summary));
    item.insert("detail".into(), json!(detail));
    vec![item]
}

/// `/name args` for a slash command the user typed, else None.
fn command_text(text: &str) -> Option<String> {
    let name = COMMAND_NAME.captures(text)?;
    let mut cmd = name[1].trim().to_string();
    if !cmd.starts_with('/') {
        cmd.insert(0, '/');
    }
    let args = COMMAND_ARGS
        .captures(text)
        .map(|a| a[1].trim().to_string())
        .unwrap_or_default();
    Some(format!("{cmd} {args}").trim().to_string())
}

fn user_text_items(text: &str, base: &Item) -> Vec<Item> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    if trimmed.starts_with("<command-") {
        if let Some(command) = command_text(trimmed) {
            return vec![with(base, json!({"kind": "user", "text": command}))];
        }
    }
    if NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return vec![];
    }
    if trimmed.starts_with("[Request interrupted by user") {
        return vec![with(base, json!({"kind": "note", "text": "Interrupted"}))];
    }
    if trimmed.starts_with("<task-notification>") {
        let summary = SUMMARY_TAG.captures(trimmed).map(|c| c[1].to_string());
        let text = one_line(
            summary.as_deref().unwrap_or("Background task update"),
            SUMMARY_LIMIT,
        );
        return vec![with(base, json!({"kind": "note", "text": text}))];
    }
    vec![with(
        base,
        json!({"kind": "user", "text": cut(text, TEXT_LIMIT)}),
    )]
}

fn truthy(v: &Value, key: &str) -> bool {
    match v.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        _ => true,
    }
}

/// Display items for one transcript line (often none).
pub fn parse_line(raw: &[u8]) -> Vec<Item> {
    if let Some(caps) = LEADING_TYPE.captures(raw) {
        if !matches!(&caps[1], b"user" | b"assistant" | b"system") {
            return vec![];
        }
    }
    let Ok(d) = serde_json::from_slice::<Value>(raw) else {
        return vec![];
    };
    if !d.is_object() {
        return vec![];
    }
    let kind = s(&d, "type").unwrap_or("");
    if !matches!(kind, "user" | "assistant" | "system")
        || truthy(&d, "isSidechain")
        || truthy(&d, "isMeta")
    {
        return vec![];
    }
    let uid = s(&d, "uuid").unwrap_or("");
    let mut base = Item::new();
    base.insert("id".into(), json!(uid));
    base.insert("ts".into(), json!(s(&d, "timestamp").unwrap_or("")));
    if kind == "system" {
        if s(&d, "subtype") == Some("compact_boundary") {
            return vec![with(
                &base,
                json!({"kind": "note", "text": "Conversation compacted"}),
            )];
        }
        return vec![];
    }
    if truthy(&d, "isCompactSummary") {
        return vec![];
    }
    let blocks: Vec<Value> = match d.pointer("/message/content") {
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        _ => vec![],
    };
    let mut items = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        if !block.is_object() {
            continue;
        }
        let bbase = with(&base, json!({"id": format!("{uid}:{i}")}));
        let btype = s(block, "type").unwrap_or("");
        if kind == "user" {
            match btype {
                "text" => items.extend(user_text_items(s(block, "text").unwrap_or(""), &bbase)),
                "image" => items.push(with(&bbase, json!({"kind": "user", "text": "[image]"}))),
                "tool_result" => {
                    let text = result_text(block.get("content").unwrap_or(&Value::Null));
                    items.push(with(
                        &bbase,
                        json!({
                            "kind": "result",
                            "call_id": s(block, "tool_use_id").unwrap_or(""),
                            "is_error": truthy(block, "is_error"),
                            "text": cut(&text, RESULT_LIMIT),
                        }),
                    ));
                }
                _ => {}
            }
        } else if btype == "text" {
            let text = s(block, "text").unwrap_or("");
            if !text.trim().is_empty() {
                items.push(with(
                    &bbase,
                    json!({"kind": "assistant", "text": cut(text, TEXT_LIMIT)}),
                ));
            }
        } else if btype == "tool_use" {
            items.extend(tool_items(block, &bbase));
        }
    }
    items
}

fn parse_lines<'a>(lines: impl Iterator<Item = &'a [u8]>) -> Vec<Item> {
    lines
        .filter(|l| !l.is_empty())
        .flat_map(parse_line)
        .collect()
}

fn read_at(fh: &mut File, pos: u64, len: u64) -> io::Result<Vec<u8>> {
    fh.seek(SeekFrom::Start(pos))?;
    let mut buf = Vec::with_capacity(len as usize);
    fh.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Items from complete lines ending at byte offset `before` (default: end of file).
///
/// Reads backwards until at least `want` displayable items (not counting tool
/// results) are found, the file starts, or MAX_BACK_SCAN bytes were read.
/// Returns the items in file order with `start` (offset of the first line
/// read; pass it as `before` for the previous page) and `end`.
pub fn read_tail(path: &Path, before: Option<u64>, want: usize, chunk: u64) -> io::Result<Value> {
    let mut fh = File::open(path)?;
    let size = fh.metadata()?.len();
    let mut end = before.map_or(size, |b| b.min(size));
    if before.is_none() {
        // Ignore a final line that is still being written: end after the last newline.
        let mut pos = end;
        end = 0;
        while pos > 0 {
            let step = chunk.min(pos);
            let buf = read_at(&mut fh, pos - step, step)?;
            if let Some(cut) = buf.iter().rposition(|&b| b == b'\n') {
                end = pos - step + cut as u64 + 1;
                break;
            }
            pos -= step;
        }
    }
    let mut pos = end;
    let mut carry: Vec<u8> = Vec::new();
    let mut pages: Vec<Vec<Item>> = Vec::new();
    let mut count = 0;
    let mut scanned = 0;
    while pos > 0 && count < want && scanned < MAX_BACK_SCAN {
        let step = chunk.min(pos);
        pos -= step;
        let mut buf = read_at(&mut fh, pos, step)?;
        buf.extend_from_slice(&carry);
        scanned += step;
        let lines: Vec<&[u8]> = buf.split(|&b| b == b'\n').collect();
        let (body, new_carry): (&[&[u8]], Vec<u8>) = if pos > 0 {
            (&lines[1..], lines[0].to_vec())
        } else {
            (&lines[..], Vec::new())
        };
        let items = parse_lines(body.iter().copied());
        count += items
            .iter()
            .filter(|it| it.get("kind").and_then(Value::as_str) != Some("result"))
            .count();
        pages.push(items);
        carry = new_carry;
    }
    let mut start = pos + carry.len() as u64;
    if pos == 0 && !carry.is_empty() {
        pages.push(parse_line(&carry));
        start = 0;
    }
    let items: Vec<Item> = pages.into_iter().rev().flatten().collect();
    Ok(json!({"items": items, "start": start, "end": end}))
}

/// Items from complete lines after byte `offset`, and the offset to continue from.
pub fn read_since(path: &Path, offset: u64) -> io::Result<Value> {
    let mut fh = File::open(path)?;
    let size = fh.metadata()?.len();
    if offset > size || size - offset > MAX_FORWARD_READ {
        // Truncated, replaced, or too far behind: start over from the tail.
        let mut tail = read_tail(path, None, 80, CHUNK)?;
        tail["reset"] = json!(true);
        return Ok(tail);
    }
    let buf = read_at(&mut fh, offset, size - offset)?;
    let Some(cut) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(json!({"items": [], "end": offset}));
    };
    let items = parse_lines(buf[..cut].split(|&b| b == b'\n'));
    Ok(json!({"items": items, "end": offset + cut as u64 + 1}))
}

/// cmux's ChatAgentState description -> idle | working | needs_input | ended.
pub fn session_state(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("working") {
        "working".into()
    } else if raw.starts_with("needsInput") || raw.starts_with("needs_input") {
        "needs_input".into()
    } else if raw.starts_with("ended") {
        "ended".into()
    } else if raw.starts_with("idle") {
        "idle".into()
    } else if raw.is_empty() {
        "unknown".into()
    } else {
        raw.into()
    }
}
