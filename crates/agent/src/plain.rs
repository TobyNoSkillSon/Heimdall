//! Plain-text tool results. Every Heimdall MCP tool answers the model in short lines, never raw JSON (
//! "The amount of just useless fucking JSON that is just pasted into you is absolutely ridiculous … this has to be
//! much nicer interface like actually really fucking simple"). Content is never shortened here: a reply, a question
//! or a transcript passes in full; only structure (braces, quotes, nulls, internal bookkeeping) is dropped.
use chrono::TimeZone;
use serde_json::Value;

/// A JSON value as indented `key: value` lines. Nulls and empty values are left out, a list of plain values is one
/// comma-separated line, a list of objects is `- ` items, multi-line text keeps its lines. Nothing is shortened.
pub fn render(v: &Value) -> String {
    match v {
        Value::Object(_) | Value::Array(_) => {
            let mut out = String::new();
            write(&mut out, v, 0);
            let text = out.trim_end().to_owned();
            if text.is_empty() { "(empty)".into() } else { text }
        }
        other => scalar(other).unwrap_or_else(|| "(empty)".into()),
    }
}

/// `text` unchanged, unless it is a JSON object or array: then its `render`.
pub fn text(text: &str) -> String {
    let t = text.trim_start();
    if t.starts_with('{') || t.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            if v.is_object() || v.is_array() {
                return render(&v);
            }
        }
    }
    text.to_owned()
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.values().all(empty),
        _ => false,
    }
}
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}
/// A list that reads as one line: plain values without line breaks.
fn inline_list(items: &[Value]) -> Option<String> {
    let parts: Option<Vec<String>> = items.iter().filter(|x| !empty(x)).map(|x| scalar(x).filter(|s| !s.contains('\n'))).collect();
    parts.map(|p| p.join(", "))
}
fn push_text(out: &mut String, pad: &str, head: &str, text: &str) {
    if text.contains('\n') {
        out.push_str(&format!("{pad}{head}\n"));
        for line in text.lines() {
            out.push_str(&format!("{pad}  {line}\n"));
        }
    } else {
        out.push_str(&format!("{pad}{head} {text}\n"));
    }
}
fn write(out: &mut String, v: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        Value::Object(map) => {
            for (key, x) in map.iter().filter(|(_, x)| !empty(x)) {
                if let Some(s) = scalar(x) {
                    push_text(out, &pad, &format!("{key}:"), &s);
                } else if let Some(line) = x.as_array().and_then(|a| inline_list(a)) {
                    out.push_str(&format!("{pad}{key}: {line}\n"));
                } else {
                    out.push_str(&format!("{pad}{key}:\n"));
                    write(out, x, indent + 2);
                }
            }
        }
        Value::Array(items) => {
            for x in items.iter().filter(|x| !empty(x)) {
                if let Some(s) = scalar(x) {
                    push_text(out, &pad, "-", &s);
                    continue;
                }
                // The item's own lines, with its first line marked as a list entry.
                let mut item = String::new();
                write(&mut item, x, indent + 2);
                let item = item.replacen(&format!("{pad}  "), &format!("{pad}- "), 1);
                out.push_str(&item);
            }
        }
        other => {
            if let Some(s) = scalar(other) {
                push_text(out, &pad, "", &s);
            }
        }
    }
}

/// A timestamp from the archive (RFC 3339, any offset) in `zone`, to the second: the form the transcript uses and the
/// one `timeline_search` accepts back as `start`/`end`.
pub fn time_in<Tz: TimeZone>(utc: &str, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    chrono::DateTime::parse_from_rfc3339(utc)
        .map(|t| t.with_timezone(zone).to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        .unwrap_or_else(|_| utc.to_owned())
}

/// `time_in` this Mac's time zone (what the user and the transcript use).
pub fn local_time(utc: &str) -> String {
    time_in(utc, &chrono::Local)
}

/// A timestamp as a hit line shows it: day, month and minute in `zone` ("26 Oct 00:55").
pub fn short_time_in<Tz: TimeZone>(utc: &str, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    chrono::DateTime::parse_from_rfc3339(utc)
        .map(|t| t.with_timezone(zone).format("%-d %b %H:%M").to_string())
        .unwrap_or_else(|_| utc.to_owned())
}

/// `timeline_search` as lines ("this needs to be a lot more token efficient"): one line per
/// distinct moment, `26 Oct 00:55 · Mic · the user · 0.82 · [h3] By the way… (+412 chars)`. The handle goes back as
/// `id`; `output: "full"` adds the whole text, the full id, merged and related ids under the line. A detail page
/// (`id`) is the line's head and the text. The continuation is one line.
pub fn timeline<Tz: TimeZone>(v: &Value, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    let detail = v["search_mode"] == "detail";
    let mut lines: Vec<String> = vec![];
    if hits.is_empty() {
        lines.push("No hits.".into());
    }
    for h in &hits {
        let mut head = vec![h["start"].as_str().map(|t| short_time_in(t, zone)).unwrap_or_default()];
        if let Some(l) = h["label"].as_str().filter(|l| !l.is_empty()) { head.push(l.to_owned()); }
        if let Some(s) = h["similarity"].as_f64() { head.push(format!("{s:.2}")); }
        if h["pending_embedding"] == true { head.push("recent, literal match (not yet searchable by meaning)".into()); }
        let handle = h["h"].as_str().or(h["id"].as_str()).unwrap_or("");
        let text = h["text"].as_str().unwrap_or("");
        if detail {
            let continued = h["text_offset_bytes"].as_u64().is_some_and(|o| o > 0);
            lines.push(format!("{} · [{handle}]", head.join(" · ")));
            lines.push(if continued { format!("…{text}") } else { text.to_owned() });
            continue;
        }
        let more = h["more"].as_u64().map(|n| format!(" (+{n} chars)")).unwrap_or_default();
        let full = h.get("id").is_some();
        if full {
            lines.push(format!("{} · [{handle}] {}", head.join(" · "), h["id"].as_str().unwrap_or("")));
            for line in text.lines() { lines.push(format!("  {line}")); }
            if !more.is_empty() { lines.push(format!("  …{}", more.trim_start())); }
            for (key, name) in [("merged", "merged"), ("related_ids", "related")] {
                if let Some(list) = h[key].as_array().and_then(|a| inline_list(a)).filter(|s| !s.is_empty()) {
                    lines.push(format!("  {name}: {list}"));
                }
            }
        } else {
            lines.push(format!("{} · [{handle}] {}{more}", head.join(" · "), text.replace('\n', " ")).trim_end().to_owned());
        }
    }
    if let Some(c) = v["next_cursor"].as_str() {
        lines.push(format!("More: cursor {c}"));
    }
    lines.join("\n")
}

/// One `screen_frames` image's metadata as a line (it precedes the image it describes).
pub fn frame<Tz: TimeZone>(v: &Value, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let mut parts = vec![format!("Frame {}", v["frame_id"].as_str().unwrap_or("?"))];
    if let Some(t) = v["capture"].as_str() { parts.push(format!("captured {}", time_in(t, zone))); }
    if let Some(d) = v["display_id"].as_str() { parts.push(format!("display {d}")); }
    if let (Some(w), Some(h)) = (v["width"].as_u64(), v["height"].as_u64()) { parts.push(format!("{w}×{h} px")); }
    if let Some(q) = v["quality"].as_str() { parts.push(q.to_owned()); }
    if v["archived"] == true { parts.push("archived".into()); }
    parts.join(" · ")
}

/// Every text block of a tool result in plain form (the MCP boundary's guarantee for every tool): JSON text becomes
/// lines, `screen_frames` metadata becomes one line per frame. Image blocks are untouched.
pub fn result_blocks<Tz: TimeZone>(result: &mut Value, tool: &str, zone: &Tz)
where
    Tz::Offset: std::fmt::Display,
{
    let Some(blocks) = result["content"].as_array_mut() else { return };
    for block in blocks.iter_mut().filter(|b| b["type"] == "text") {
        let Some(t) = block["text"].as_str() else { continue };
        let trimmed = t.trim_start();
        if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(t) else { continue };
        let plain = if tool == "screen_frames" && v.get("frame_id").is_some() {
            frame(&v, zone)
        } else if tool == "timeline_search" && v.get("hits").is_some() {
            timeline(&v, zone)
        } else if v.is_object() || v.is_array() {
            render(&v)
        } else {
            continue;
        };
        block["text"] = Value::String(plain);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn cet() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(2 * 3600).unwrap()
    }
    #[test]
    fn render_is_indented_lines_without_braces_quotes_or_nulls() {
        let v = json!({"threadId":"t-1","status":"completed","activeRunId":null,"tags":["a","b"],"items":[{"position":2,"text":"Hello\nsecond line","kind":"assistant_message"},{"position":3,"text":"","kind":"user_message"}],"empty":{},"none":[]});
        assert_eq!(
            render(&v),
            "items:\n  - kind: assistant_message\n    position: 2\n    text:\n      Hello\n      second line\n  - kind: user_message\n    position: 3\nstatus: completed\ntags: a, b\nthreadId: t-1"
        );
        assert_eq!(render(&json!({})), "(empty)");
        assert_eq!(render(&json!("plain")), "plain");
        assert_eq!(render(&json!([1, 2])), "- 1\n- 2");
        assert_eq!(render(&json!({"sequence":1})), "sequence: 1");
    }
    #[test]
    fn render_keeps_every_character_of_long_unicode_text() {
        let long = "Zażółć gęślą jaźń 😀 ".repeat(2000);
        let out = render(&json!({"text": long}));
        assert!(out.contains(long.trim_end()), "text was shortened");
        assert!(!out.contains('{') && !out.contains('"'));
    }
    #[test]
    fn text_converts_only_json() {
        assert_eq!(text("Speech queued: 42."), "Speech queued: 42.");
        assert_eq!(text("{\"a\":1}"), "a: 1");
        assert_eq!(text("{not json"), "{not json");
        assert_eq!(text("[x] done"), "[x] done");
    }
    /// One short line per distinct moment, the handle in brackets, the More line one line.
    #[test]
    fn timeline_golden_lines_full_detail_and_empty() {
        let lines = json!({"search_mode":"semantic","hits":[
            {"h":"h3","start":"2026-10-25T23:55:13Z","end":"2026-10-25T23:55:40Z","label":"Mic · User","similarity":0.8213,"text":"By the way, the Iron Man 2 centre channel… catch all the Jarvis lines…","more":412},
            {"h":"h4","start":"2026-10-14T18:21:49Z","label":"Discord · Mohawk","similarity":0.8,"text":"Jarvis from The Iron Man."},
            {"h":"h5","start":"2026-10-26T00:15:00Z","label":"Screen · Ego Lite","similarity":0.78}
        ],"next_cursor":"c7"});
        assert_eq!(timeline(&lines, &cet()),
            "26 Oct 01:55 · Mic · User · 0.82 · [h3] By the way, the Iron Man 2 centre channel… catch all the Jarvis lines… (+412 chars)\n14 Oct 20:21 · Discord · Mohawk · 0.80 · [h4] Jarvis from The Iron Man.\n26 Oct 02:15 · Screen · Ego Lite · 0.78 · [h5]\nMore: cursor c7");
        let full = json!({"search_mode":"literal","hits":[{"h":"h3","id":"56:text:turn:s:241","start":"2026-10-25T23:55:13Z","label":"Mic · User","text":"one\ntwo","more":10,"merged":["56:speech_audio:turn:s:241"],"related_ids":["frame-9"]}],"next_cursor":null});
        assert_eq!(timeline(&full, &cet()), "26 Oct 01:55 · Mic · User · [h3] 56:text:turn:s:241\n  one\n  two\n  …(+10 chars)\n  merged: 56:speech_audio:turn:s:241\n  related: frame-9");
        let detail = json!({"search_mode":"detail","hits":[{"h":"h3","id":"x","start":"2026-10-19T10:00:00Z","label":"Mic · User","text":"rest","text_offset_bytes":3000}],"next_cursor":null});
        assert_eq!(timeline(&detail, &cet()), "19 Oct 12:00 · Mic · User · [h3]\n…rest");
        assert_eq!(timeline(&json!({"search_mode":"literal","hits":[],"next_cursor":null}), &cet()), "No hits.");
        let pending = json!({"search_mode":"semantic","hits":[{"h":"h1","start":"2026-10-19T10:00:00Z","label":"Mic · User","text":"just said","pending_embedding":true}],"next_cursor":null});
        assert_eq!(timeline(&pending, &cet()), "19 Oct 12:00 · Mic · User · recent, literal match (not yet searchable by meaning) · [h1] just said");
    }
    #[test]
    fn frame_and_result_blocks_golden() {
        let meta = json!({"frame_id":"screen:1:2:3","capture":"2026-10-19T10:30:07Z","coverage_at":"2026-10-19T10:30:07Z","display_id":"1","display":{"x":0,"y":0,"width":2,"height":1},"width":2,"height":1,"quality":"preview","archived":true,"source_tier":"hot"});
        let mut result = json!({"content":[{"type":"text","text":meta.to_string()},{"type":"image","data":"AQEB","mimeType":"image/jpeg"},{"type":"text","text":"Seeing is off."}]});
        result_blocks(&mut result, "screen_frames", &cet());
        assert_eq!(result["content"][0]["text"], "Frame screen:1:2:3 · captured 2026-10-19T12:30:07+02:00 · display 1 · 2×1 px · preview · archived");
        assert_eq!(result["content"][1]["data"], "AQEB");
        assert_eq!(result["content"][2]["text"], "Seeing is off.");
        let mut other = json!({"content":[{"type":"text","text":"{\"status\":\"archived\",\"worker_id\":\"w\"}"}]});
        result_blocks(&mut other, "anything", &cet());
        assert_eq!(other["content"][0]["text"], "status: archived\nworker_id: w");
    }
}
