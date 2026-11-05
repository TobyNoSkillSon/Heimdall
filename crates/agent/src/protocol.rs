use anyhow::{ensure, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
/// Complete UTF-8 JSON lines, including frames split inside a multibyte character.
#[derive(Default)]
pub struct Framer {
    pending: Vec<u8>,
    scanned: usize,
}
impl Framer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        ensure!(
            self.pending.len() <= 64 * 1024 * 1024,
            "harness frame exceeds 64 MiB"
        );
        let mut out = Vec::new();
        while let Some(end) = self.pending[self.scanned..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + self.scanned)
        {
            let line: Vec<_> = self.pending.drain(..=end).collect();
            self.scanned = 0;
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            out.push(serde_json::from_slice(&line)?);
        }
        self.scanned = self.pending.len();
        Ok(out)
    }
    pub fn finish(&mut self) -> Result<()> {
        ensure!(
            self.pending.iter().all(|b| b.is_ascii_whitespace()),
            "truncated harness frame at EOF"
        );
        self.pending.clear();
        self.scanned = 0;
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub timestamp_ms: u64,
    pub source: String,
    pub speaker: String,
    pub language: String,
    pub text: String,
}
impl Observation {
    pub fn labelled(&self) -> String {
        self.text.split('\n').map(|line| {
            // Speech splitting represents a mixed call as one parent observation
            // containing named runs. Give each run its own compact speaker label.
            let (speaker, text) = if self.speaker == "Call" {
                line.split_once(": ").unwrap_or((&self.speaker, line))
            } else { (self.speaker.as_str(), line) };
            format!("[{} · {}] {}", compact_source(&self.source), speaker, text)
        }).collect::<Vec<_>>().join("\n")
    }
}
fn compact_source(source: &str) -> &str {
    if source == "mic" || source == "microphone" || source == "Microphone" || source.starts_with("mic:") { "Mic" } else { source }
}

/// Decode only the old structured prefix, never look for ids inside spoken text.
/// Kept for queued observations and archived sessions from previous builds.
pub fn legacy_observation(line: &str) -> Option<Observation> {
    let (id, rest) = line.strip_prefix("event=")?.split_once(" timestamp_ms=")?;
    let (timestamp, rest) = rest.split_once(" source=")?;
    fn quoted(rest: &str) -> Option<(String, &str)> {
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let value = values.next()?.ok()?;
        Some((value, &rest[values.byte_offset()..]))
    }
    let (source, rest) = quoted(rest)?;
    let (speaker, rest) = quoted(rest.strip_prefix(" speaker=")?)?;
    let (language, text) = rest.strip_prefix(" lang=")?.split_once(": ")?;
    Some(Observation { id:id.into(), timestamp_ms:timestamp.parse().ok()?, source, speaker, language:language.into(), text:text.into() })
}
pub fn compact_transcript(text: &str) -> String {
    let (data, instruction) = crate::identity::split_wake(text).map_or((text, None), |(data, instruction)| (data, Some(instruction)));
    let mut compact = data.split('\n').map(|line| legacy_observation(line).map(|o|o.labelled()).unwrap_or_else(||line.into())).collect::<Vec<_>>().join("\n");
    if let Some(instruction) = instruction { compact.push_str(crate::identity::WAKE_MARKER); compact.push_str(instruction); }
    compact
}
/// Observation ownership belongs to the durable envelope, outside model text.
/// Legacy fallback is limited to text blocks with the exact old prefix.
pub fn observation_ids(wire: &Value) -> Vec<String> {
    if let Some(ids) = wire["observation_ids"].as_array() {
        return ids.iter().filter_map(Value::as_str).map(str::to_owned).collect();
    }
    wire["message"]["content"].as_array().into_iter().flatten()
        .filter_map(|p|p["text"].as_str()).flat_map(|text|crate::identity::split_wake(text).map_or(text, |(data, _)| data).lines())
        .filter_map(legacy_observation).map(|o|o.id).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub media_type: String,
    pub bytes: Vec<u8>,
}
impl Attachment {
    /// The wire block. Its media type is the bytes' own (`history::sniff`) whatever the producer declared: a
    /// mislabelled image is refused by the provider and stays in the history (20 Oct 19:20).
    pub fn block(&self) -> Result<Value> {
        let media_type = crate::history::sniff(&self.bytes).unwrap_or(self.media_type.as_str());
        ensure!(
            matches!(
                media_type,
                "image/png" | "image/jpeg" | "image/webp" | "image/gif"
            ),
            "unsupported image type"
        );
        ensure!(
            !self.bytes.is_empty() && self.bytes.len() <= 20 * 1024 * 1024,
            "invalid image size"
        );
        use sha2::Digest;
        let archive_id = format!("{:x}", sha2::Sha256::digest(&self.bytes));
        Ok(
            json!({"type":"image","source":{"type":"base64","media_type":media_type,"archive_image_id":archive_id,"data":base64::engine::general_purpose::STANDARD.encode(&self.bytes)}}),
        )
    }
}
/// Inline screenshots stay bounded; overflow remains addressable in the image CAS.
pub const WAKE_IMAGES: usize = 8;
pub fn bound_wake_images(wire: &mut Value) {
    let mut overflow = wire["screenshot_overflow"].as_array().cloned().unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut count = 0;
    if let Some(content) = wire["message"]["content"].as_array_mut() {
        content.retain(|part| {
            if part["type"] != "image" { return true; }
            let id = part["source"]["archive_image_id"].as_str().unwrap_or("").to_owned();
            if !seen.insert(id.clone()) { return false; }
            count += 1;
            if count <= WAKE_IMAGES { true } else {
                if !overflow.iter().any(|v|v == &id) { overflow.push(json!(id)); }
                false
            }
        });
    }
    let inline: std::collections::HashSet<_> = wire["message"]["content"].as_array().into_iter().flatten()
        .filter_map(|p|p["source"]["archive_image_id"].as_str()).collect();
    overflow.retain(|v|v.as_str().is_some_and(|id|!inline.contains(id)));
    wire["screenshot_overflow"] = json!(overflow);
}
pub fn describe_overflow_images(wire: &mut Value) {
    if let Some(ids) = wire["screenshot_overflow"].as_array().filter(|a|!a.is_empty()) {
        let note = format!("\n[Additional screenshots retained in the archive (content IDs): {}]",ids.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "));
        if let Some(text) = wire["message"]["content"][0]["text"].as_str() {
            wire["message"]["content"][0]["text"] = json!(format!("{text}{note}"));
        }
    }
}
pub fn message(
    id: &str,
    text: &str,
    images: &[Attachment],
    query: bool,
    urgent: bool,
) -> Result<Value> {
    let mut content = vec![json!({"type":"text","text":text})];
    for image in images {
        content.push(image.block()?);
    }
    Ok(
        json!({"type":"user","uuid":id,"timestamp_ms":crate::now_ms(),"message":{"role":"user","content":content},"parent_tool_use_id":null,"observation_ids":[],"shouldQuery":query,"priority":if urgent{"now"}else{"next"}}),
    )
}
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Init {
        session: String,
        capabilities: Vec<String>,
        api_key_source: Option<String>,
    },
    Echo {
        id: String,
    },
    /// The harness put this exact user message into the model's context.
    /// Acceptance alone (Echo) does not establish consumption.
    Consumed {
        id: String,
    },
    Complete {
        id: Option<String>,
        query: bool,
        error: bool,
        text: String,
    },
    Limit {
        rejected: bool,
        reset: Option<u64>,
        status: String,
    },
    Deferred {
        id: String,
        reason: String,
    },
    Other(Value),
}
