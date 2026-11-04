//! Images the provider would refuse, found before they reach it (night pass 20→21 Oct).
//!
//! 20 Oct 19:20:55: `look` returned a JPEG labelled `image/png` (`crates/app/src/drawing.rs` hardcoded the type
//! while `context_attachment` switched to JPEG above 1.5 MB). Anthropic refused every later request with
//! `messages.N.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media
//! type, but the image appears to be a image/jpeg image`; the compaction kept the image in its tail, and every reload
//! resumed the same history, so the agent never answered again.
//!
//! Two defences live here, both from the documented image rules
//! (platform.claude.com/docs/en/build-with-claude/vision, read 20 Oct 2026): PNG, JPEG, GIF or WebP; at most 5 MB
//! base64 (the direct API takes 10 MB, partner platforms 5 MB; the stricter one is kept); at most 8000 px a side, and
//! at most 2000 px a side when a request carries more than 20 images.
//! - `sniff`/`normalize_tool_images`: every image leaving Heimdall is labelled from its own bytes.
//! - `repair`: at every agent start, before Pi reads the session, an image on the active branch that breaks a rule
//!   is relabelled (wrong type) or replaced by a one-line note (unreadable, too large). The fix is Pi's own
//!   append-only `context_edit` entry: the original entry stays in the file, only the model's view changes. A copy of
//!   the file is kept beside it first. The running agent repairs itself the same way (`runtime/history-repair.mjs`).
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
};

/// The largest image the provider takes, base64-encoded.
pub const MAX_IMAGE_BASE64: usize = 5 * 1024 * 1024;
/// The longest side of any image.
pub const MAX_SIDE: u32 = 8000;
/// More images than this in one request lower the longest side to `MANY_IMAGES_SIDE`.
pub const MANY_IMAGES: usize = 20;
pub const MANY_IMAGES_SIDE: u32 = 2000;

/// An image's real format and size, from its bytes. A size of 0 means the header did not say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
}

/// The media type the bytes are, if they are an image the provider accepts.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    image_info(bytes).map(|i| i.media_type)
}

/// Format and dimensions from the file header (PNG, JPEG, GIF, WebP).
pub fn image_info(b: &[u8]) -> Option<ImageInfo> {
    let be16 = |i: usize| b.get(i..i + 2).map(|s| u16::from_be_bytes([s[0], s[1]]) as u32);
    let le16 = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u32);
    let le24 = |i: usize| b.get(i..i + 3).map(|s| s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16);
    let info = |media_type, size: Option<(u32, u32)>| {
        let (width, height) = size.unwrap_or((0, 0));
        Some(ImageInfo { media_type, width, height })
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let size = b.get(16..24).map(|s| (u32::from_be_bytes([s[0], s[1], s[2], s[3]]), u32::from_be_bytes([s[4], s[5], s[6], s[7]])));
        return info("image/png", size);
    }
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        // Walk the markers to the first frame header (SOF0–SOF15 except DHT, JPG, DAC).
        let mut i = 2;
        let mut size = None;
        while i + 3 < b.len() {
            if b[i] != 0xff { break; }
            let marker = b[i + 1];
            if marker == 0xff { i += 1; continue; }
            if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) { i += 2; continue; }
            let Some(len) = be16(i + 2) else { break };
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                size = be16(i + 7).zip(be16(i + 5));
                break;
            }
            if marker == 0xda || len < 2 { break; }
            i += 2 + len as usize;
        }
        return info("image/jpeg", size);
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return info("image/gif", le16(6).zip(le16(8)));
    }
    if b.len() >= 16 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let size = match &b[12..16] {
            b"VP8 " => le16(26).zip(le16(28)).map(|(w, h)| (w & 0x3fff, h & 0x3fff)),
            b"VP8L" => b.get(21..25).map(|s| {
                let w = 1 + (s[0] as u32 | ((s[1] as u32) & 0x3f) << 8);
                let h = 1 + ((s[1] as u32) >> 6 | (s[2] as u32) << 2 | ((s[3] as u32) & 0x0f) << 10);
                (w, h)
            }),
            b"VP8X" => le24(24).zip(le24(27)).map(|(w, h)| (w + 1, h + 1)),
            _ => None,
        };
        return info("image/webp", size);
    }
    None
}

/// `sniff` on base64 data, reading only its first bytes (enough for every signature).
pub fn sniff_base64(data: &str) -> Option<&'static str> {
    let head = &data.as_bytes()[..data.len().min(64) / 4 * 4];
    sniff(&decode(std::str::from_utf8(head).ok()?)?)
}

fn decode(data: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    STANDARD.decode(data).or_else(|_| STANDARD_NO_PAD.decode(data)).ok()
}

/// What an image block needs before the provider will take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fix {
    /// The bytes are fine; the label is wrong.
    Relabel { from: String, to: &'static str },
    /// The image cannot go: replaced by a one-line note saying why.
    Remove(&'static str),
}
impl Fix {
    /// The note that takes a removed image's place.
    pub fn note(reason: &str) -> String {
        format!("(Image removed by Heimdall: {reason}.)")
    }
}

/// Checks one base64 image against the documented rules. `many`: the request carries more than `MANY_IMAGES`.
pub fn check_image(media_type: &str, data: &str, many: bool) -> Option<Fix> {
    if data.len() > MAX_IMAGE_BASE64 {
        return Some(Fix::Remove("larger than the model accepts"));
    }
    let Some(bytes) = decode(data) else { return Some(Fix::Remove("its data was unreadable")) };
    let Some(info) = image_info(&bytes) else { return Some(Fix::Remove("not a PNG, JPEG, GIF or WebP image")) };
    let side = info.width.max(info.height);
    if side > MAX_SIDE {
        return Some(Fix::Remove("wider or taller than 8000 pixels"));
    }
    if many && side > MANY_IMAGES_SIDE {
        return Some(Fix::Remove("over 2000 pixels in a conversation with more than 20 images"));
    }
    (info.media_type != media_type).then(|| Fix::Relabel { from: media_type.to_owned(), to: info.media_type })
}

/// An MCP tool result's image blocks (`{"type":"image","data","mimeType"}`) labelled from their bytes. Every Heimdall
/// tool's result passes here (`mcp.rs`). Only the label changes: an image the provider refuses for another reason is
/// repaired where the refusal is seen (`runtime/history-repair.mjs`) or at the next start (`repair`).
pub fn normalize_tool_images(result: &mut Value) {
    let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) else { return };
    for block in content.iter_mut().filter(|b| b["type"] == "image") {
        if let Some(real) = sniff_base64(block["data"].as_str().unwrap_or_default()) {
            if block["mimeType"] != real { block["mimeType"] = json!(real); }
        }
    }
}

/// What a start's repair changed, for one plain log line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repair {
    /// (labelled as, really) per relabelled image.
    pub relabelled: Vec<(String, String)>,
    /// Why, per removed image.
    pub removed: Vec<String>,
    /// Session entries given a `context_edit`.
    pub entries: Vec<String>,
    /// The file's last line had no line break (a stop mid-write): `Some(true)` it was a whole entry and was ended,
    /// `Some(false)` it was cut off and was removed.
    pub unfinished_line: Option<bool>,
    pub backup: Option<PathBuf>,
}
impl Repair {
    pub fn line(&self) -> String {
        let mut parts = vec![];
        match self.unfinished_line {
            Some(true) => parts.push("the last line was never ended (Heimdall stopped mid-write); ended it".to_owned()),
            Some(false) => parts.push("the last line was cut off (Heimdall stopped mid-write); removed it".to_owned()),
            None => {}
        }
        let mut kinds: Vec<(String, usize)> = vec![];
        for (from, to) in &self.relabelled {
            let kind = format!("labelled {from} but {to}");
            match kinds.iter_mut().find(|(k, _)| *k == kind) { Some((_, n)) => *n += 1, None => kinds.push((kind, 1)) }
        }
        for (kind, n) in kinds {
            parts.push(format!("{n} image{} {kind}, relabelled", if n == 1 { "" } else { "s" }));
        }
        let mut reasons: Vec<(String, usize)> = vec![];
        for reason in &self.removed {
            match reasons.iter_mut().find(|(r, _)| r == reason) { Some((_, n)) => *n += 1, None => reasons.push((reason.clone(), 1)) }
        }
        for (reason, n) in reasons {
            parts.push(format!("{n} image{} removed ({reason})", if n == 1 { "" } else { "s" }));
        }
        format!("Repaired the conversation before the provider saw it: {}", parts.join("; "))
    }
}

/// The backup a repair keeps beside the session: one copy of the file as it was before, replaced by the next repair.
pub fn backup_path(session: &Path) -> PathBuf {
    let mut name = session.file_name().unwrap_or_default().to_os_string();
    name.push(".repair-backup");
    session.with_file_name(name)
}

/// The session entries the model sees, as Pi 1.2.0 builds them (`buildContextEntries`, `buildSessionProjection`):
/// the active branch from the last entry, the latest compaction and its kept range, each target's latest
/// `context_edit` applied. Returns (entry id, role, content) for every entry whose content can carry images.
fn visible(entries: &[Value]) -> Vec<(String, &'static str, Value)> {
    let by_id: HashMap<&str, &Value> = entries.iter().filter_map(|e| e["id"].as_str().map(|id| (id, e))).collect();
    let mut path = vec![];
    let mut seen = HashSet::new();
    let mut cursor = entries.last().and_then(|e| e["id"].as_str());
    while let Some(id) = cursor {
        if !seen.insert(id) { break; }
        let Some(e) = by_id.get(id) else { break };
        path.push(*e);
        cursor = e["parentId"].as_str();
    }
    path.reverse();
    let context: Vec<&Value> = match path.iter().rposition(|e| e["type"] == "compaction") {
        None => path,
        Some(at) => {
            let kept = path[at]["firstKeptEntryId"].as_str().unwrap_or_default();
            let start = path[..at].iter().position(|e| e["id"] == kept).unwrap_or(at);
            path[start..at].iter().filter(|e| !(e["type"] == "message" && e["message"]["role"] == "system"))
                .chain(path[at + 1..].iter()).copied().collect()
        }
    };
    let mut edits: HashMap<&str, &Value> = HashMap::new();
    for e in &context {
        if e["type"] == "context_edit" {
            if let Some(target) = e["targetId"].as_str() { edits.insert(target, &e["replacement"]); }
        }
    }
    let mut out = vec![];
    for e in context {
        let Some(id) = e["id"].as_str() else { continue };
        let (role, content) = match e["type"].as_str() {
            Some("message") => match e["message"]["role"].as_str() {
                Some("user") => ("user", &e["message"]["content"]),
                Some("toolResult") => ("toolResult", &e["message"]["content"]),
                _ => continue,
            },
            Some("custom_message") => ("custom", &e["content"]),
            _ => continue,
        };
        let content = match edits.get(id) {
            Some(Value::Null) => continue,
            Some(replacement) => &replacement["content"],
            None => content,
        };
        if content.is_array() { out.push((id.to_owned(), role, content.clone())); }
    }
    out
}

/// The content with every image the provider would refuse fixed, if any was.
fn fixed(content: &Value, many: bool, report: &mut Repair) -> Option<Value> {
    let mut changed = false;
    let blocks = content.as_array()?.iter().map(|block| {
        if block["type"] != "image" { return block.clone(); }
        let media_type = block["mimeType"].as_str().unwrap_or_default();
        match check_image(media_type, block["data"].as_str().unwrap_or_default(), many) {
            Some(Fix::Relabel { from, to }) => {
                changed = true;
                report.relabelled.push((from, to.into()));
                let mut block = block.clone();
                block["mimeType"] = json!(to);
                block
            }
            Some(Fix::Remove(reason)) => {
                changed = true;
                report.removed.push(reason.into());
                json!({"type":"text","text":Fix::note(reason)})
            }
            None => block.clone(),
        }
    }).collect::<Vec<_>>();
    changed.then_some(Value::Array(blocks))
}

/// Checks a Pi session before Pi reads it, and repairs it with a copy of the file kept first (`backup_path`):
/// - a last line without a line break (Heimdall or Pi stopped mid-write) is ended when it is a whole entry and cut
///   away when it is not, so the next entry Pi or Heimdall appends starts on its own line and no later reader
///   (`session::rotate`, which rewrites the file) meets a broken line in the middle;
/// - every image the model would see and refuse gets one `context_edit` per affected entry.
///
/// Only while no Pi holds the session (the harness calls it under the session lock, before starting Pi). `None` when
/// nothing needed repair.
pub fn repair(session: &Path) -> Result<Option<Repair>> {
    let original = match std::fs::read(session) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut report = Repair::default();
    let backup = backup_path(session);
    let mut bytes = original.clone();
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        crate::runtime::atomic_write(&backup, &original).context("history repair backup")?;
        report.backup = Some(backup.clone());
        let start = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let whole = serde_json::from_slice::<Value>(&bytes[start..]).is_ok_and(|v| v.is_object());
        let file = std::fs::OpenOptions::new().append(true).open(session)?;
        if whole {
            (&file).write_all(b"\n")?;
            bytes.push(b'\n');
        } else {
            file.set_len(start as u64)?;
            bytes.truncate(start);
        }
        file.sync_all()?;
        report.unfinished_line = Some(whole);
    }
    // Most starts: no image in the file at all (Pi writes compact JSON).
    const IMAGE: &[u8] = b"\"type\":\"image\"";
    let images = bytes.windows(IMAGE.len()).any(|w| w == IMAGE);
    if !images && report.unfinished_line.is_none() { return Ok(None); }
    let mut entries = vec![];
    let mut header = None;
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)) {
        // Pi skips a line it cannot parse; so does this.
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        if v["type"] == "session" { header.get_or_insert(v); continue; }
        entries.push(v);
    }
    let finished = report.unfinished_line.is_some();
    let Some(header) = header else { return Ok(finished.then_some(report)) };
    if header["version"] != 3 { return Ok(finished.then_some(report)); }
    let mut edits = vec![];
    if images {
        let context = visible(&entries);
        let many = context.iter().flat_map(|(_, _, c)| c.as_array().into_iter().flatten()).filter(|b| b["type"] == "image").count() > MANY_IMAGES;
        for (id, _, content) in &context {
            if let Some(content) = fixed(content, many, &mut report) {
                report.entries.push(id.clone());
                edits.push((id.clone(), content));
            }
        }
    }
    if edits.is_empty() && !finished { return Ok(None); }
    if report.backup.is_none() {
        crate::runtime::atomic_write(&backup, &original).context("history repair backup")?;
        report.backup = Some(backup);
    }
    let mut ids: HashSet<String> = entries.iter().filter_map(|e| e["id"].as_str().map(str::to_owned)).collect();
    let mut parent = entries.last().and_then(|e| e["id"].as_str()).map(str::to_owned);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut text = String::new();
    if !bytes.ends_with(b"\n") { text.push('\n'); }
    for (target, content) in edits {
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
            if ids.insert(id.clone()) { break id; }
        };
        let entry = json!({"type":"context_edit","id":id,"parentId":parent,"timestamp":now,"targetId":target,"replacement":{"content":content}});
        text.push_str(&entry.to_string());
        text.push('\n');
        parent = Some(id);
    }
    // The line for the Agent window, as the running agent's repair leaves it (`runtime/history-repair.mjs`).
    let id = loop {
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        if ids.insert(id.clone()) { break id; }
    };
    let note = json!({"type":"custom","id":id,"parentId":parent,"timestamp":now,"customType":"heimdall_history_repair","data":{"state":"repaired_at_start","line":report.line()}});
    text.push_str(&note.to_string());
    text.push('\n');
    let mut file = std::fs::OpenOptions::new().append(true).open(session)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
        b
    }
    pub(crate) fn jpeg(w: u16, h: u16) -> Vec<u8> {
        // SOI, APP0 (JFIF, 16 bytes), DQT stub, SOF0 with height then width.
        let mut b = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        b.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        b.extend([0xff, 0xdb, 0x00, 0x04, 0x00, 0x00]);
        b.extend([0xff, 0xc0, 0x00, 0x11, 0x08]);
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([0x03, 0x01, 0x22, 0x00]);
        b
    }

    #[test]
    fn formats_and_sizes_come_from_the_bytes() {
        assert_eq!(image_info(&png(1728, 1117)), Some(ImageInfo { media_type: "image/png", width: 1728, height: 1117 }));
        assert_eq!(image_info(&jpeg(1728, 1117)), Some(ImageInfo { media_type: "image/jpeg", width: 1728, height: 1117 }));
        let mut gif = b"GIF89a".to_vec();
        gif.extend([0x40, 0x01, 0xc8, 0x00]);
        assert_eq!(image_info(&gif), Some(ImageInfo { media_type: "image/gif", width: 320, height: 200 }));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        webp.extend([0; 8]);
        webp.extend([0x3f, 0x01, 0x00, 0xc7, 0x00, 0x00]); // 320 − 1, 200 − 1
        assert_eq!(image_info(&webp), Some(ImageInfo { media_type: "image/webp", width: 320, height: 200 }));
        assert_eq!(image_info(b"%PDF-1.7"), None);
        assert_eq!(image_info(&[0xff, 0xd8, 0xff]).map(|i| i.media_type), Some("image/jpeg"), "a truncated JPEG is still a JPEG");
        // The real 20 Oct screenshot started like this: JFIF, labelled PNG.
        assert_eq!(sniff(&STANDARD.decode("/9j/4AAQSkZJRgABAQAAAQABAAD/").unwrap()), Some("image/jpeg"));
        assert_eq!(sniff_base64(&STANDARD.encode(jpeg(1728, 1117))), Some("image/jpeg"));
        assert_eq!(sniff_base64(&STANDARD.encode(png(1, 1))), Some("image/png"));
        assert_eq!(sniff_base64("AQID"), None);
        assert_eq!(sniff_base64(""), None);
    }

    #[test]
    fn each_documented_rule_has_its_fix() {
        let b64 = |b: Vec<u8>| STANDARD.encode(b);
        assert_eq!(check_image("image/png", &b64(png(1728, 1117)), false), None);
        assert_eq!(check_image("image/png", &b64(jpeg(1728, 1117)), false), Some(Fix::Relabel { from: "image/png".into(), to: "image/jpeg" }));
        assert_eq!(check_image("image/jpeg", &b64(png(8001, 10)), false), Some(Fix::Remove("wider or taller than 8000 pixels")));
        assert_eq!(check_image("image/png", &b64(png(2001, 10)), false), None);
        assert!(matches!(check_image("image/png", &b64(png(2001, 10)), true), Some(Fix::Remove(_))));
        assert_eq!(check_image("image/png", "not base64 at all!", false), Some(Fix::Remove("its data was unreadable")));
        assert_eq!(check_image("image/png", &b64(b"%PDF-1.7".to_vec()), false), Some(Fix::Remove("not a PNG, JPEG, GIF or WebP image")));
        assert_eq!(check_image("image/png", &"A".repeat(MAX_IMAGE_BASE64 + 4), false), Some(Fix::Remove("larger than the model accepts")));
    }

    #[test]
    fn tool_results_leave_labelled_from_their_bytes() {
        let mut result = json!({"content":[
            {"type":"image","mimeType":"image/png","data":STANDARD.encode(jpeg(1728, 1117))},
            {"type":"image","mimeType":"image/png","data":STANDARD.encode(png(10, 10))},
            {"type":"image","mimeType":"image/png","data":STANDARD.encode(b"<html>")},
            {"type":"text","text":"Display 1: size 1728 1117"},
        ]});
        let unreadable = result["content"][2].clone();
        normalize_tool_images(&mut result);
        assert_eq!(result["content"][0]["mimeType"], "image/jpeg");
        assert_eq!(result["content"][1]["mimeType"], "image/png");
        assert_eq!(result["content"][2], unreadable, "only labels change here");
        assert_eq!(result["content"][3]["type"], "text");
    }

    /// The 20 Oct shape, shortened: a compaction that kept the tail from before the bad `look`, the tool result with
    /// a JPEG labelled PNG, and the failed requests after it.
    pub(crate) fn broken_session(path: &Path) -> Vec<Value> {
        let jpeg_png = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(jpeg(1728, 1117))});
        let good_png = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(png(1728, 1117))});
        let rows = vec![
            json!({"type":"session","version":3,"id":"s","timestamp":"2026-10-14T20:24:54.925Z","cwd":"/"}),
            json!({"type":"message","id":"old","parentId":null,"message":{"role":"toolResult","toolCallId":"t0","content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(b"summarised away")}]}}),
            json!({"type":"message","id":"kept","parentId":"old","message":{"role":"user","content":[{"type":"text","text":"look at this"},good_png]}}),
            json!({"type":"message","id":"call","parentId":"kept","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"mcp__heimdall__look","arguments":{}}]}}),
            json!({"type":"message","id":"shot","parentId":"call","message":{"role":"toolResult","toolCallId":"t1","toolName":"mcp__heimdall__look","content":[jpeg_png,{"type":"text","text":"Display 1: size 1728 1117"}]}}),
            json!({"type":"message","id":"err1","parentId":"shot","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"400 messages.345.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image"}}),
            json!({"type":"compaction","id":"c","parentId":"err1","firstKeptEntryId":"kept","summary":"earlier","tokensBefore":112652,"systemMessage":{"role":"system","content":""}}),
            json!({"type":"custom_message","id":"ctx","parentId":"c","customType":"heimdall_context","content":"Heimdall, you have just been restarted","display":true}),
            json!({"type":"message","id":"wake","parentId":"ctx","message":{"role":"user","content":[{"type":"text","text":"[Heimdall delivery: d]\nHi Heimdall, you there?"}]}}),
            json!({"type":"message","id":"err2","parentId":"wake","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"400 messages.118.content.0.tool_result.content.0.image.source.base64: …"}}),
            json!({"type":"custom","id":"rcpt","parentId":"err2","customType":"heimdall_receipt","data":{"id":"d","completed":false}}),
        ];
        std::fs::write(path, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        rows
    }
    fn faults(path: &Path) -> usize {
        let entries: Vec<Value> = std::fs::read_to_string(path).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).filter(|v: &Value| v["type"] != "session").collect();
        let ctx = visible(&entries);
        let many = ctx.iter().flat_map(|(_, _, c)| c.as_array().unwrap().clone()).filter(|b| b["type"] == "image").count() > MANY_IMAGES;
        ctx.iter().flat_map(|(_, _, c)| c.as_array().unwrap().clone())
            .filter(|b| b["type"] == "image" && check_image(b["mimeType"].as_str().unwrap(), b["data"].as_str().unwrap(), many).is_some()).count()
    }

    #[test]
    fn the_20_oct_session_is_repaired_at_start_with_a_backup_and_nothing_else_touched() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("session.jsonl");
        broken_session(&p);
        let before = std::fs::read(&p).unwrap();
        assert_eq!(faults(&p), 1);
        let report = repair(&p).unwrap().expect("a repair");
        assert_eq!(report.relabelled, vec![("image/png".to_owned(), "image/jpeg".to_owned())]);
        assert!(report.removed.is_empty(), "the image summarised away is not in the model's view and is left alone");
        assert_eq!(report.entries, vec!["shot".to_owned()]);
        assert_eq!(report.line(), "Repaired the conversation before the provider saw it: 1 image labelled image/png but image/jpeg, relabelled");
        assert_eq!(std::fs::read(backup_path(&p)).unwrap(), before);
        let after = std::fs::read(&p).unwrap();
        assert!(after.starts_with(&before), "append-only: every original line stays as it was");
        let added: Vec<Value> = std::str::from_utf8(&after[before.len()..]).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(added.len(), 2);
        assert_eq!(added[1]["customType"], "heimdall_history_repair");
        assert_eq!(added[1]["parentId"], added[0]["id"]);
        assert_eq!(added[1]["data"]["line"], report.line());
        assert_eq!(added[0]["type"], "context_edit");
        assert_eq!(added[0]["parentId"], "rcpt", "chained from the last entry, so Pi's leaf stays on the branch");
        assert_eq!(added[0]["targetId"], "shot");
        assert_eq!(added[0]["replacement"]["content"][0]["mimeType"], "image/jpeg");
        assert_eq!(added[0]["replacement"]["content"][1]["text"], "Display 1: size 1728 1117", "everything else in the result kept");
        assert_eq!(faults(&p), 0);
        // The next start finds nothing to do and writes nothing.
        assert_eq!(repair(&p).unwrap(), None);
        assert_eq!(std::fs::read(&p).unwrap(), after);
    }

    #[test]
    fn images_in_wakes_and_context_messages_too_and_a_removed_image_leaves_a_note() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let rows = [
            json!({"type":"session","version":3,"id":"s"}),
            json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","mimeType":"image/jpeg","data":STANDARD.encode(png(9000, 10))}]}}),
            json!({"type":"custom_message","id":"m","parentId":"u","customType":"x","content":[{"type":"image","mimeType":"image/webp","data":STANDARD.encode(png(5, 5))}]}),
        ];
        std::fs::write(&p, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let report = repair(&p).unwrap().unwrap();
        assert_eq!(report.removed, vec!["wider or taller than 8000 pixels".to_owned()]);
        assert_eq!(report.relabelled, vec![("image/webp".to_owned(), "image/png".to_owned())]);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("(Image removed by Heimdall: wider or taller than 8000 pixels.)"));
        assert_eq!(faults(&p), 0);
    }

    #[test]
    fn more_than_twenty_images_hold_every_image_to_2000_pixels() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let mut rows = vec![json!({"type":"session","version":3,"id":"s"})];
        let mut parent = Value::Null;
        for n in 0..21 {
            let side = if n == 3 { 2576 } else { 1000 };
            rows.push(json!({"type":"message","id":format!("u{n}"),"parentId":parent,"message":{"role":"user","content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(png(side, 600))}]}}));
            parent = json!(format!("u{n}"));
        }
        std::fs::write(&p, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let report = repair(&p).unwrap().unwrap();
        assert_eq!(report.entries, vec!["u3".to_owned()]);
        assert_eq!(faults(&p), 0);
        // Twenty images: the 2576-px one is within the 8000-px rule.
        let p2 = d.path().join("s2.jsonl");
        std::fs::write(&p2, rows[..21].iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        assert_eq!(repair(&p2).unwrap(), None);
    }

    #[test]
    fn an_earlier_edit_is_respected_and_other_branches_and_summarised_entries_are_not_touched() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let bad = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(jpeg(10, 10))});
        let rows = [
            json!({"type":"session","version":3,"id":"s"}),
            json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":[bad.clone()]}}),
            // An abandoned branch from `a`, with its own bad image.
            json!({"type":"message","id":"side","parentId":"a","message":{"role":"user","content":[bad.clone()]}}),
            json!({"type":"message","id":"b","parentId":"a","message":{"role":"user","content":[bad.clone()]}}),
            // Pi omitted `b` from the model's view already.
            json!({"type":"context_edit","id":"e","parentId":"b","targetId":"b","replacement":null}),
            json!({"type":"message","id":"c","parentId":"e","message":{"role":"user","content":[bad]}}),
        ];
        std::fs::write(&p, rows.iter().map(|r| format!("{r}\n")).collect::<String>()).unwrap();
        let report = repair(&p).unwrap().unwrap();
        assert_eq!(report.entries, vec!["a".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn a_session_without_images_or_without_a_v3_header_is_never_rewritten() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        assert_eq!(repair(&p).unwrap(), None, "no file yet");
        std::fs::write(&p, "{\"type\":\"session\",\"version\":3,\"id\":\"s\"}\n{\"type\":\"message\",\"id\":\"a\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();
        assert_eq!(repair(&p).unwrap(), None);
        let bad = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(jpeg(10, 10))});
        std::fs::write(&p, format!("{}\n{}\n", json!({"type":"session","version":2}), json!({"type":"message","id":"a","message":{"role":"user","content":[bad]}}))).unwrap();
        assert_eq!(repair(&p).unwrap(), None);
        assert!(!backup_path(&p).exists());
    }

    /// Night review 21 Oct (item 6): a stop mid-write leaves a last line without a line break. Every start ends it
    /// (a whole entry) or cuts it away (a broken one), after a backup, even when nothing else needs repair; the next
    /// start finds nothing to do.
    #[test]
    fn an_unfinished_last_line_is_ended_or_cut_at_start_with_a_backup() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        let head = format!("{}\n{}\n", json!({"type":"session","version":3,"id":"s"}), json!({"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}));
        // Cut off half-way through an entry: removed.
        let cut = format!("{head}{{\"type\":\"message\",\"id\":\"b\",\"parentId\":\"a\",\"message\":{{\"role\":\"assi");
        std::fs::write(&p, &cut).unwrap();
        let report = repair(&p).unwrap().expect("repaired");
        assert_eq!(report.unfinished_line, Some(false));
        assert_eq!(report.line(), "Repaired the conversation before the provider saw it: the last line was cut off (Heimdall stopped mid-write); removed it");
        assert_eq!(std::fs::read_to_string(backup_path(&p)).unwrap(), cut, "the file as it was");
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with(&head) && after.ends_with('\n'), "{after}");
        let added: Vec<Value> = after[head.len()..].lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(added.len(), 1, "only the Agent window's line: {added:?}");
        assert_eq!((added[0]["customType"].as_str(), added[0]["parentId"].as_str()), (Some("heimdall_history_repair"), Some("a")));
        assert_eq!(repair(&p).unwrap(), None, "the next start has nothing to do");
        // A whole entry without its line break: ended, kept.
        let whole = format!("{head}{}", json!({"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":[]}}));
        std::fs::write(&p, &whole).unwrap();
        let report = repair(&p).unwrap().expect("repaired");
        assert_eq!(report.unfinished_line, Some(true));
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.starts_with(&format!("{whole}\n")), "{after}");
        assert_eq!(after.lines().last().map(|l| serde_json::from_str::<Value>(l).unwrap()["parentId"].clone()), Some(json!("b")), "chained after the kept entry");
        // A cut-off line together with an image to repair: one backup of the original, both repaired.
        let bad = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(jpeg(10, 10))});
        let both = format!("{head}{}\n{{\"type\":\"mess", json!({"type":"message","id":"c","parentId":"a","message":{"role":"user","content":[bad]}}));
        std::fs::write(&p, &both).unwrap();
        let report = repair(&p).unwrap().expect("repaired");
        assert_eq!((report.unfinished_line, report.entries.clone()), (Some(false), vec!["c".to_owned()]));
        assert_eq!(std::fs::read_to_string(backup_path(&p)).unwrap(), both);
        assert_eq!(faults(&p), 0);
        assert!(std::fs::read_to_string(&p).unwrap().lines().all(|l| serde_json::from_str::<Value>(l).is_ok()), "every line whole");
        // A file that is only a cut-off header is emptied; Pi starts it afresh.
        std::fs::write(&p, "{\"type\":\"sess").unwrap();
        assert_eq!(repair(&p).unwrap().map(|r| r.unfinished_line), Some(Some(false)));
        assert_eq!(std::fs::read(&p).unwrap(), b"");
    }

    /// Rotation (`session::rotate`) keeps a repair: the edit and its target are both in the kept tail.
    #[test]
    fn a_repaired_session_still_rotates_with_its_edit() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.jsonl");
        broken_session(&p);
        repair(&p).unwrap().unwrap();
        assert!(crate::session::rotate(&p, 0, |_| Ok(())).unwrap());
        assert_eq!(faults(&p), 0);
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"context_edit\""));
    }

    /// A copy of a real session (never the profile's own file): `HEIMDALL_REPAIR_SESSION=<copy> cargo test -p agent
    /// --lib history::tests::a_copy_of_a_real_session -- --ignored --nocapture`. The copy is repaired in place, so Pi's
    /// own projection can check it afterwards (`lab/agent/scripts/check_session_images.mjs`).
    #[test]
    #[ignore = "explicit: HEIMDALL_REPAIR_SESSION names a copy of a real session"]
    fn a_copy_of_a_real_session() {
        let p = PathBuf::from(std::env::var("HEIMDALL_REPAIR_SESSION").unwrap());
        assert!(!p.starts_with(dirs_profile()), "a copy, never the profile's session");
        let before = faults(&p);
        let started = std::time::Instant::now();
        let report = repair(&p).unwrap();
        println!("faults before {before}, after {}; {:?} in {:?}", faults(&p), report.as_ref().map(Repair::line), started.elapsed());
        assert_eq!(faults(&p), 0);
    }
    fn dirs_profile() -> PathBuf {
        heimdall_home::data_dir()
    }
}
