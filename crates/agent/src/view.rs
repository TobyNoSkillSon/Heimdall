//! Pi RPC and persisted-session views. Streaming updates replace stable entries;
//! the compatibility view emits finalized text for the existing read-only window.
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Prompt { query: bool },
    Reply,
    ToolCall { name: String },
    ToolResult { name: String, error: bool },
    Failure,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub time_ms: u64,
    pub kind: Kind,
    pub text: String,
}
#[derive(Default, Debug)]
pub struct SessionView {
    pub session_id: Option<String>,
    pub model: Option<String>,
    loop_stopped: bool,
}
impl SessionView {
    pub fn sent(&mut self, v: &Value, time_ms: u64) -> Vec<Entry> {
        if v["id"].as_str().is_some_and(|s| s.starts_with("control-")) {
            return vec![];
        }
        let text = if v["type"] == "user" {
            message_text(&v["message"]["content"])
        } else if v["type"] == "prompt" {
            let mut t = clean(v["message"].as_str().unwrap_or(""));
            for _ in v["images"].as_array().into_iter().flatten() {
                t.push_str("\n[image]");
            }
            t
        } else {
            return vec![];
        };
        if text.is_empty() {
            vec![]
        } else {
            vec![Entry {
                time_ms,
                kind: Kind::Prompt {
                    query: v["shouldQuery"].as_bool().unwrap_or(true),
                },
                text,
            }]
        }
    }
    pub fn received(&mut self, v: &Value, t: u64) -> Vec<Entry> {
        match v["type"].as_str() {
            Some("agent_settled") => { self.loop_stopped = false; vec![] },
            Some("message_end") if v["message"]["role"] == "user" => { self.loop_stopped = false; vec![] },
            Some("entry_appended") if loop_stop_entry(&v["entry"]) => {
                self.loop_stopped = true;
                vec![Entry { time_ms: t, kind: Kind::Failure, text: crate::harness::loop_policy().notice.clone() }]
            },
            Some("message_end") if v["message"]["role"] == "assistant" => {
                self.model = v["message"]["model"]
                    .as_str()
                    .map(str::to_owned)
                    .or(self.model.take());
                let mut e = vec![];
                let text = message_text(&v["message"]["content"]);
                if !text.is_empty() {
                    e.push(Entry {
                        time_ms: t,
                        kind: Kind::Reply,
                        text,
                    });
                }
                if matches!(
                    v["message"]["stopReason"].as_str(),
                    Some("error" | "aborted")
                ) && !(self.loop_stopped && v["message"]["stopReason"] == "aborted") {
                    e.push(Entry {
                        time_ms: t,
                        kind: Kind::Failure,
                        text: failure_text(&v["message"]),
                    });
                }
                e
            }
            Some("tool_execution_start") => vec![Entry {
                time_ms: t,
                kind: Kind::ToolCall {
                    name: short_tool(v["toolName"].as_str().unwrap_or("tool")),
                },
                text: v["args"].to_string(),
            }],
            Some("tool_execution_end") => vec![Entry {
                time_ms: t,
                kind: Kind::ToolResult {
                    name: short_tool(v["toolName"].as_str().unwrap_or("tool")),
                    error: v["isError"] == true,
                },
                text: message_text(&v["result"]["content"]),
            }],
            Some("response") if v["success"] == false => vec![Entry {
                time_ms: t,
                kind: Kind::Failure,
                text: v["error"].as_str().unwrap_or("Agent request failed").into(),
            }],
            Some("response") if v["command"] == "get_state" => {
                self.session_id = v["data"]["sessionId"].as_str().map(str::to_owned);
                self.model = v["data"]["model"]["id"].as_str().map(str::to_owned);
                vec![]
            }
            _ => vec![],
        }
    }
    pub fn history_line(&mut self, v: &Value) -> Vec<Entry> {
        if !v["would_send"].is_null() {
            return self.sent(&v["would_send"], v["timestamp_ms"].as_u64().unwrap_or(0));
        }
        if v["type"] == "session" {
            self.session_id = v["id"].as_str().map(str::to_owned);
            return vec![];
        }
        if loop_stop_entry(v) {
            self.loop_stopped = true;
            return vec![Entry { time_ms: v["timestamp"].as_str().and_then(iso_ms).unwrap_or(0),
                kind: Kind::Failure, text: crate::harness::loop_policy().notice.clone() }];
        }
        if v["type"] != "message" {
            return vec![];
        }
        let m = &v["message"];
        let t = v["timestamp"].as_str().and_then(iso_ms).unwrap_or(0);
        if m["role"] == "user" {
            self.loop_stopped = false;
            return self.sent(
                &serde_json::json!({"type":"prompt","message":message_text(&m["content"])}),
                t,
            );
        }
        if m["role"] == "toolResult" {
            return self.received(&serde_json::json!({"type":"tool_execution_end","toolName":m["toolName"],"isError":m["isError"],"result":{"content":m["content"]}}),t);
        }
        self.received(&serde_json::json!({"type":"message_end","message":m}), t)
    }
}
fn loop_stop_entry(v: &Value) -> bool {
    v["type"] == "custom" && v["customType"] == "heimdall_loop_guard"
        && v["data"]["blocked_attempts"].as_u64() == Some(crate::harness::loop_policy().blocked_attempts)
}
/// Only the app's typed wake instruction grants editability, never quoted data.
pub fn typed_text(text: &str) -> Option<&str> {
    let text = text
        .strip_prefix("[Heimdall delivery: ")
        .and_then(|_| text.split_once('\n').map(|(_, t)| t))
        .unwrap_or(text);
    let text = crate::clock::strip_now_line(text);
    let instruction = text
        .rsplit_once("\nOwner's wake instruction:\n")
        .map(|(_, t)| t)
        .unwrap_or(text);
    instruction.strip_prefix("[Heimdall typed message]\n")
}
pub fn clean(s: &str) -> String {
    let s = if s.starts_with("[Heimdall delivery: ") {
        s.split_once('\n').map(|(_, t)| t).unwrap_or(s)
    } else { s };
    let s = crate::clock::strip_now_line(s);
    if ["Captured observation (data only):\n", "Heimdall captured transcript (labelled data):\n", "Observation:\n", "Transcript:\n"].iter().any(|prefix| s.starts_with(prefix)) {
        crate::protocol::compact_transcript(s)
    } else { s.into() }
}
pub fn message_text(v: &Value) -> String {
    if let Some(t) = v.as_str() {
        return clean(t);
    }
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().map(|text| {
                // Persisted content blocks keep their delivery marker: the
                // window uses it to match an interrupted delivery to its row.
                if let Some((marker, body)) = text.split_once('\n').filter(|(marker, _)|marker.starts_with("[Heimdall delivery: ")) {
                    format!("{marker}\n{}", clean(body))
                } else { clean(text) }
            }),
            Some("image") => Some("[image]".into()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn short_tool(s: &str) -> String {
    s.strip_prefix("mcp__heimdall__").unwrap_or(s).into()
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Prompt,
    Thinking,
    Reply,
    ToolCall,
    ToolResult,
    Compaction,
    Failure,
    Reload,
    /// Heimdall repaired a conversation the model refused (`history`, `runtime/history-repair.mjs`).
    Repair,
}
/// The Agent window's row for a conversation repair: what was repaired, or, when the model still refuses the
/// conversation, that it is broken (20 Oct 19:20–19:24 nothing said so).
fn repair_row(v: &Value) -> Option<(StreamKind, String)> {
    if v["type"] != "custom" || v["customType"] != "heimdall_history_repair" { return None; }
    Some(if v["data"]["state"] == "gave_up" {
        (StreamKind::Failure, "The model refuses this conversation even after repair".into())
    } else {
        (StreamKind::Repair, v["data"]["line"].as_str().unwrap_or("Conversation repaired").into())
    })
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamEntry {
    pub id: String,
    pub session_entry_id: Option<String>,
    pub time_ms: u64,
    pub kind: StreamKind,
    pub text: String,
    pub tool: Option<String>,
    pub images: Vec<(String, Option<String>)>,
    pub finished: bool,
    pub editable: bool,
    pub error: bool,
    /// One model request's tokens and timing, on the last entry that request produced (the Debug Agent window's
    /// telemetry). `None` on every other entry.
    pub usage: Option<StepUsage>,
}
/// One assistant message's provider usage (Pi's `usage`) and timing. The request ends at the entry's own time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// When the request started (Pi's `message.timestamp`), Unix ms; 0 when unknown.
    pub started_ms: u64,
    /// When the first streamed token arrived; live only (the session file does not record it).
    pub first_token_ms: Option<u64>,
}
impl StepUsage {
    /// Pi's assistant message `usage`, or `None` when the message has none.
    pub fn from_message(m: &Value) -> Option<Self> {
        let u = m.get("usage").filter(|u| u.is_object())?;
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        Some(Self { input: n("input"), output: n("output"), cache_read: n("cacheRead"), cache_write: n("cacheWrite"),
            started_ms: m["timestamp"].as_u64().unwrap_or(0), first_token_ms: None })
    }
}
#[derive(Default, Debug)]
pub struct LiveView {
    message: u64,
    blocks: HashMap<usize, StreamEntry>,
    loop_stopped: bool,
    /// The streaming message's start and first token (receipt times), for its usage's timing.
    started_ms: u64,
    first_token_ms: Option<u64>,
    /// A tool-only message's usage, carried to its first tool call.
    pending_usage: Option<StepUsage>,
}
impl LiveView {
    fn entry(&self, id: String, kind: StreamKind, text: String, time_ms: u64) -> StreamEntry {
        StreamEntry {
            id,
            session_entry_id: None,
            time_ms,
            error: matches!(kind, StreamKind::Failure),
            kind,
            text,
            tool: None,
            images: vec![],
            finished: true,
            editable: false,
            usage: None,
        }
    }
    pub fn sent(&mut self, v: &Value, t: u64) -> Vec<StreamEntry> {
        if !matches!(v["type"].as_str(), Some("prompt" | "user"))
            || v["id"].as_str().is_some_and(|s| s.starts_with("control-"))
        {
            return vec![];
        }
        let text = if v["type"] == "user" {
            message_text(&v["message"]["content"])
        } else {
            clean(v["message"].as_str().unwrap_or(""))
        };
        let mut e = self.entry(
            v["id"]
                .as_str()
                .or_else(|| v["uuid"].as_str())
                .unwrap_or("")
                .into(),
            StreamKind::Prompt,
            text,
            t,
        );
        let images = if v["type"] == "user" {
            &v["message"]["content"]
        } else {
            &v["images"]
        };
        e.images = images
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "image")
            .map(image_meta)
            .collect();
        e.editable = typed_text(&e.text).is_some();
        vec![e]
    }
    pub fn received(&mut self, v: &Value, t: u64) -> Vec<StreamEntry> {
        match v["type"].as_str() {
            Some("agent_settled") => { self.loop_stopped = false; vec![] },
            Some("message_end") if v["message"]["role"] == "user" => { self.loop_stopped = false; vec![] },
            Some("message_end") if v["message"]["role"] == "custom" && v["message"]["customType"] == "heimdall_context" => vec![self
                .entry(format!("context-{t}"), StreamKind::Prompt, message_text(&v["message"]["content"]), t)],
            Some("entry_appended") if loop_stop_entry(&v["entry"]) => {
                self.loop_stopped = true;
                vec![self.entry(format!("{}-0", v["entry"]["id"].as_str().unwrap_or("loop-stop")),
                    StreamKind::Failure, crate::harness::loop_policy().notice.clone(), t)]
            },
            Some("message_start") if v["message"]["role"] == "assistant" => {
                self.message += 1;
                self.blocks.clear();
                self.started_ms = v["message"]["timestamp"].as_u64().unwrap_or(t);
                self.first_token_ms = None;
                vec![]
            }
            Some("message_update") => {
                self.first_token_ms.get_or_insert(t);
                let a = &v["assistantMessageEvent"];
                let i = a["contentIndex"].as_u64().unwrap_or(0) as usize;
                let kind = match a["type"].as_str() {
                    Some("thinking_start" | "thinking_delta" | "thinking_end") => {
                        StreamKind::Thinking
                    }
                    Some("text_start" | "text_delta" | "text_end") => StreamKind::Reply,
                    _ => return vec![],
                };
                let default = self.entry(
                    format!("stream-{}-{i}", self.message),
                    kind,
                    String::new(),
                    t,
                );
                let e = self.blocks.entry(i).or_insert(default);
                if let Some(s) = a["delta"].as_str() {
                    if e.text.len() + s.len() <= 1024 * 1024 {
                        e.text.push_str(s);
                    }
                }
                if let Some(s) = a["content"].as_str() {
                    e.text = s.chars().take(1024 * 1024).collect();
                }
                e.finished = a["type"].as_str().is_some_and(|k| k.ends_with("_end"));
                vec![e.clone()]
            }
            Some("message_end") if v["message"]["role"] == "assistant" => {
                let mut out = vec![];
                for (i, b) in v["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let kind = match b["type"].as_str() {
                        Some("thinking") => StreamKind::Thinking,
                        Some("text") => StreamKind::Reply,
                        _ => continue,
                    };
                    let text = b["text"]
                        .as_str()
                        .or_else(|| b["thinking"].as_str())
                        .unwrap_or("")
                        .into();
                    let e = self.entry(format!("stream-{}-{i}", self.message), kind, text, t);
                    out.push(e);
                }
                self.blocks.clear();
                if let Some(mut usage) = StepUsage::from_message(&v["message"]) {
                    if usage.started_ms == 0 { usage.started_ms = self.started_ms; }
                    usage.first_token_ms = self.first_token_ms.take();
                    match out.last_mut() { Some(last) => last.usage = Some(usage), None => self.pending_usage = Some(usage) }
                }
                if matches!(
                    v["message"]["stopReason"].as_str(),
                    Some("error" | "aborted")
                ) && !(self.loop_stopped && v["message"]["stopReason"] == "aborted") {
                    let mut e = self.entry(format!("failure-{}", self.message), StreamKind::Failure, failure_text(&v["message"]), t);
                    // A stop is not an error (`failure_text`).
                    e.error = v["message"]["stopReason"] != "aborted";
                    out.push(e);
                }
                out
            }
            Some("tool_execution_start" | "tool_execution_update" | "tool_execution_end") => {
                let typ = v["type"].as_str().unwrap();
                let name = short_tool(v["toolName"].as_str().unwrap_or("tool"));
                let id = v["toolCallId"].as_str().unwrap_or("tool");
                let result = typ != "tool_execution_start";
                let value = if typ == "tool_execution_update" {
                    &v["partialResult"]
                } else {
                    &v["result"]
                };
                let mut e = self.entry(
                    format!("{id}-{}", if result { "result" } else { "call" }),
                    if result {
                        StreamKind::ToolResult
                    } else {
                        StreamKind::ToolCall
                    },
                    if result {
                        message_text(&value["content"])
                    } else {
                        v["args"].to_string()
                    },
                    t,
                );
                e.tool = Some(name);
                if !result { e.usage = self.pending_usage.take(); }
                e.finished = typ != "tool_execution_update";
                e.error = v["isError"] == true;
                if result {
                    e.images = value["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "image")
                        .map(image_meta)
                        .collect();
                }
                vec![e]
            }
            Some("heimdall_provider_wait") => {
                let delay=v["delay_ms"].as_u64().unwrap_or(0);
                let provider=v["provider"].as_str().unwrap_or("Provider");
                let text=if delay==0 {format!("{provider} request resumed")}
                    else if delay<=90_000 {format!("{provider} rate limit · retrying in {} seconds",delay.div_ceil(1000))}
                    else {format!("{provider} rate limit · retrying at {}",clock_ms(v["until_ms"].as_u64().unwrap_or(t+delay)))};
                let mut e=self.entry("provider-rate-wait".into(),StreamKind::Compaction,text,t);
                e.finished=delay==0; vec![e]
            }
            Some("compaction_start" | "compaction_end") => {
                let mut e = self.entry(
                    format!("compaction-{}", self.message),
                    StreamKind::Compaction,
                    v["result"]["summary"]
                        .as_str()
                        .unwrap_or("Compacting session history")
                        .into(),
                    t,
                );
                e.finished = v["type"] == "compaction_end";
                vec![e]
            }
            Some("entry_appended") if repair_row(&v["entry"]).is_some() => {
                let (kind, text) = repair_row(&v["entry"]).unwrap();
                vec![self.entry(format!("{}-0", v["entry"]["id"].as_str().unwrap_or("repair")), kind, text, t)]
            }
            Some("entry_appended") if v["entry"]["customType"] == "heimdall_reload" => vec![self
                .entry(
                    v["entry"]["id"].as_str().unwrap_or("reload").into(),
                    StreamKind::Reload,
                    message_text(&v["entry"]["content"]),
                    t,
                )],
            Some("response") if v["success"] == false => vec![self.entry(
                v["id"].as_str().unwrap_or("error").into(),
                StreamKind::Failure,
                v["error"].as_str().unwrap_or("Agent request failed").into(),
                t,
            )],
            _ => vec![],
        }
    }
    pub fn history(&mut self, v: &Value) -> Vec<StreamEntry> {
        let id = v["id"].as_str().unwrap_or("");
        let t = v["timestamp"].as_str().and_then(iso_ms).unwrap_or(0);
        let m = &v["message"];
        let mut entries = if loop_stop_entry(v) {
            self.loop_stopped = true;
            vec![self.entry(id.into(), StreamKind::Failure, crate::harness::loop_policy().notice.clone(), t)]
        } else if let Some((kind, text)) = repair_row(v) {
            vec![self.entry(id.into(), kind, text, t)]
        } else if v["type"] == "compaction" {
            vec![self.entry(
                id.into(),
                StreamKind::Compaction,
                v["summary"].as_str().unwrap_or("").into(),
                t,
            )]
        } else if v["type"] == "custom_message" && v["customType"] == "heimdall_reload" {
            vec![self.entry(
                id.into(),
                StreamKind::Reload,
                message_text(&v["content"]),
                t,
            )]
        } else if v["type"] == "custom_message" && v["customType"] == "heimdall_context" {
            vec![self.entry(id.into(), StreamKind::Prompt, message_text(&v["content"]), t)]
        } else if v["type"] != "message" {
            vec![]
        } else {
            match m["role"].as_str(){
      Some("user")=>{self.loop_stopped=false;let mut e=self.entry(id.into(),StreamKind::Prompt,message_text(&m["content"]),t);e.editable=typed_text(&e.text).is_some();e.images=m["content"].as_array().into_iter().flatten().filter(|b|b["type"]=="image").map(image_meta).collect();vec![e]},
      Some("assistant")=>{self.message+=1;let mut out=self.received(&serde_json::json!({"type":"message_end","message":m}),t);for b in m["content"].as_array().into_iter().flatten().filter(|b|b["type"]=="toolCall"){let mut e=self.entry(format!("{}-call",b["id"].as_str().unwrap_or(id)),StreamKind::ToolCall,b["arguments"].to_string(),t);e.tool=Some(short_tool(b["name"].as_str().unwrap_or("tool")));out.push(e);}
        // The request's usage belongs to the last entry it produced (its tool calls come last), never carried on.
        self.pending_usage=None;let usage=out.iter_mut().find_map(|e|e.usage.take());if let (Some(mut u),Some(last))=(usage,out.last_mut()){u.first_token_ms=None;last.usage=Some(u);}out},
      Some("toolResult")=>self.received(&serde_json::json!({"type":"tool_execution_end","toolCallId":m["toolCallId"],"toolName":m["toolName"],"result":{"content":m["content"]},"isError":m["isError"]}),t),_=>vec![]
    }
        };
        for (i, e) in entries.iter_mut().enumerate() {
            e.session_entry_id = Some(id.into());
            e.id = format!("{id}-{i}");
        }
        entries
    }
}
fn image_meta(v: &Value) -> (String, Option<String>) {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let mime = v["mimeType"]
        .as_str()
        .or_else(|| v["source"]["media_type"].as_str())
        .unwrap_or("image/png")
        .to_owned();
    let reference = v["archive_image_id"]
        .as_str()
        .or_else(|| v["source"]["archive_image_id"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            v["data"]
                .as_str()
                .or_else(|| v["source"]["data"].as_str())
                .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        });
    (mime, reference)
}
/// UTC ISO timestamp to Unix ms.
pub fn iso_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(((days * 86400 + h * 3600 + min * 60 + sec) * 1000) + ms).ok()
}
/// The whole conversation's telemetry, read once when the Debug Agent window's feed opens (its rows hold only the
/// session's last entries). Counts what the session file records: tokens per request, compactions, tool calls and
/// their failures (edits apart), and failed turns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// The first entry's time; 0 for an empty session.
    pub since_ms: u64,
    /// The last entry's time: rows after it are not in these totals.
    pub through_ms: u64,
    pub requests: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The last request's prompt (input, cached and newly cached tokens): how full the context is.
    pub last_prompt: u64,
    pub compactions: u64,
    pub last_compaction_ms: Option<u64>,
    pub tool_calls: u64,
    pub tool_failures: u64,
    pub failed_edits: u64,
    pub errors: u64,
    pub last_error_ms: Option<u64>,
}
/// Tools whose failure is a failed patch: the file was not changed as the agent meant.
pub fn is_edit_tool(name: &str) -> bool {
    let name = name.rsplit("__").next().unwrap_or(name).to_ascii_lowercase();
    matches!(name.as_str(), "edit" | "write" | "multiedit" | "multi_edit" | "apply_patch" | "patch" | "str_replace" | "notebookedit")
}
/// Scans a session file line by line; only lines that can carry a count are parsed (inline images are skipped).
pub fn session_stats(path: &Path) -> anyhow::Result<SessionStats> {
    use std::io::BufRead;
    let mut out = SessionStats::default();
    for line in std::io::BufReader::new(std::fs::File::open(path)?).split(b'\n') {
        let line = line?;
        // The entry's time is near the end of every line; parse only what counts.
        let interesting = [&b"\"assistant\""[..], b"\"compaction\"", b"\"isError\":true", b"heimdall_history_repair"]
            .iter().any(|n| line.windows(n.len()).any(|w| w == *n));
        let time = |v: &Value| v["timestamp"].as_str().and_then(iso_ms);
        if !interesting {
            // Still the session's span: the timestamp of a cheap line is enough.
            if line.len() < 4096 {
                if let Some(t) = serde_json::from_slice::<Value>(&line).ok().as_ref().and_then(time) {
                    if out.since_ms == 0 { out.since_ms = t; }
                    out.through_ms = out.through_ms.max(t);
                }
            }
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        let t = time(&v).unwrap_or(0);
        if t > 0 { if out.since_ms == 0 { out.since_ms = t; } out.through_ms = out.through_ms.max(t); }
        let m = &v["message"];
        if v["type"] == "compaction" {
            out.compactions += 1;
            out.last_compaction_ms = Some(t);
        } else if v["type"] == "message" && m["role"] == "assistant" {
            if let Some(u) = StepUsage::from_message(m) {
                out.requests += 1;
                out.input += u.input; out.output += u.output; out.cache_read += u.cache_read; out.cache_write += u.cache_write;
                out.last_prompt = u.input + u.cache_read + u.cache_write;
            }
            out.tool_calls += m["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "toolCall").count() as u64;
            if matches!(m["stopReason"].as_str(), Some("error")) { out.errors += 1; out.last_error_ms = Some(t); }
        } else if v["type"] == "message" && m["role"] == "toolResult" && m["isError"] == true {
            out.tool_failures += 1;
            if is_edit_tool(m["toolName"].as_str().unwrap_or("")) { out.failed_edits += 1; }
        } else if repair_row(&v).is_some_and(|(k, _)| k == StreamKind::Failure) {
            out.errors += 1; out.last_error_ms = Some(t);
        }
    }
    Ok(out)
}
pub fn read_history(path: &Path, max_bytes: u64) -> anyhow::Result<(SessionView, Vec<Entry>)> {
    let mut f = std::fs::File::open(path)?;
    let start = f.metadata()?.len().saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![];
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        text.split_once('\n').map(|(_, t)| t).unwrap_or("")
    } else {
        &text
    };
    let mut view = SessionView::default();
    let mut entries = vec![];
    for line in text.split('\n') {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            entries.extend(view.history_line(&v));
        }
    }
    Ok((view, entries))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn stream_replaces_and_finalizes_text_and_thinking() {
        let mut v = LiveView::default();
        v.received(
            &json!({"type":"message_start","message":{"role":"assistant"}}),
            1,
        );
        let a=v.received(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"think"}}),2);
        assert_eq!(a[0].kind, StreamKind::Thinking);
        let b=v.received(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":" more"}}),3);
        assert_eq!(a[0].id, b[0].id);
        assert_eq!(b[0].text, "think more");
        assert!(!b[0].finished);
        let c=v.received(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"done"},{"type":"text","text":"reply"}]}}),4);
        assert_eq!(c[0].id, a[0].id);
        assert!(c.iter().all(|e| e.finished));
    }
    /// Audit 27 Oct, F4: Heimdall's own abort for the user's new words reached the window as "OpenAI Responses stream ended
    /// before a terminal response event", a failure. It is a stop, live and from history, on every provider; a real
    /// provider error still reads as one.
    #[test]
    fn a_steer_abort_is_a_stop_not_a_failure() {
        let aborted = json!({"role":"assistant","provider":"openai","model":"gpt-6.1-sol","stopReason":"aborted","content":[],
            "errorMessage":"OpenAI Responses stream ended before a terminal response event","usage":{"input":0,"output":0}});
        let mut v = LiveView::default();
        v.received(&json!({"type":"message_start","message":{"role":"assistant"}}), 1);
        let row = v.received(&json!({"type":"message_end","message":aborted}), 2);
        assert_eq!((row[0].kind.clone(), row[0].text.as_str(), row[0].error), (StreamKind::Failure, "Turn stopped", false));
        let row = LiveView::default().history(&json!({"type":"message","id":"m1","timestamp":"2026-10-27T02:28:46.704Z","message":aborted}));
        assert_eq!((row[0].text.as_str(), row[0].error), ("Turn stopped", false));
        let old = SessionView::default().received(&json!({"type":"message_end","message":aborted}), 3);
        assert_eq!((old[0].kind.clone(), old[0].text.as_str()), (Kind::Failure, "Turn stopped"));
        let failed = json!({"role":"assistant","provider":"anthropic","stopReason":"error","content":[],"errorMessage":"529 overloaded_error"});
        let row = LiveView::default().received(&json!({"type":"message_end","message":failed}), 4);
        assert_eq!((row[0].text.as_str(), row[0].error), ("Anthropic overloaded · try again shortly", true));
    }
    /// 20 Oct 19:20–19:24 the conversation was broken and the Agent window never said so. A repair is a row, live
    /// and in the settled history; a conversation still refused after repair says it is broken.
    #[test]
    fn a_conversation_repair_is_a_row_and_a_broken_conversation_says_so() {
        let mut v = LiveView::default();
        let repaired = json!({"type":"custom","id":"r1","customType":"heimdall_history_repair","data":{"state":"repaired","line":"The model refused the conversation; repaired it: 1 image relabelled (image/png → image/jpeg). Answering again."}});
        let row = v.history(&repaired);
        assert_eq!((row[0].kind.clone(), row[0].error), (StreamKind::Repair, false));
        assert!(row[0].text.contains("image/png → image/jpeg"));
        let live = v.received(&json!({"type":"entry_appended","entry":repaired}), 5);
        assert_eq!(live[0].kind, StreamKind::Repair);
        let broken = json!({"type":"custom","id":"r2","customType":"heimdall_history_repair","data":{"state":"gave_up","line":"The model still refuses the conversation after repair: 400 …"}});
        let row = v.history(&broken);
        assert_eq!((row[0].kind.clone(), row[0].error), (StreamKind::Failure, true));
        assert_eq!(row[0].text, "The model refuses this conversation even after repair");
        assert!(v.history(&json!({"type":"custom","id":"x","customType":"heimdall_receipt","data":{}})).is_empty());
    }
    /// The Debug Agent window's telemetry: a request's usage rides on the last entry it produced, live (with the
    /// first token's time) and from history (the request's start from `message.timestamp`, its end the entry time).
    #[test]
    fn usage_rides_on_the_last_entry_of_its_request() {
        let mut v = LiveView::default();
        v.received(&json!({"type":"message_start","message":{"role":"assistant","timestamp":1000}}), 1001);
        v.received(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hi"}}), 1800);
        v.received(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"!"}}), 1900);
        let end = v.received(&json!({"type":"message_end","message":{"role":"assistant","timestamp":1000,"content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"Hi!"}],
            "usage":{"input":2,"output":40,"cacheRead":9000,"cacheWrite":100}}}), 2500);
        assert_eq!(end[0].usage, None);
        assert_eq!(end[1].usage, Some(StepUsage { input: 2, output: 40, cache_read: 9000, cache_write: 100, started_ms: 1000, first_token_ms: Some(1800) }));
        // A tool-only request: its usage goes to the tool call that follows.
        v.received(&json!({"type":"message_start","message":{"role":"assistant"}}), 3000);
        let none = v.received(&json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"c","name":"bash","arguments":{}}],"usage":{"input":1,"output":5}}}), 3600);
        assert!(none.is_empty());
        let call = v.received(&json!({"type":"tool_execution_start","toolCallId":"c","toolName":"bash","args":{}}), 3601);
        assert_eq!(call[0].usage.as_ref().map(|u| (u.output, u.started_ms)), Some((5, 3000)));
        let result = v.received(&json!({"type":"tool_execution_end","toolCallId":"c","toolName":"bash","result":{"content":[]},"isError":false}), 3700);
        assert_eq!(result[0].usage, None);
        // History: the last entry (the tool call) carries it, with no first-token time.
        let h = v.history(&json!({"type":"message","id":"a1","timestamp":"1970-01-01T00:00:05.000Z","message":{"role":"assistant","timestamp":4000,
            "content":[{"type":"text","text":"Checking"},{"type":"toolCall","id":"c2","name":"edit","arguments":{}}],"usage":{"input":3,"output":7,"cacheRead":10,"cacheWrite":0}}}));
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].usage, None);
        assert_eq!(h[1].usage, Some(StepUsage { input: 3, output: 7, cache_read: 10, cache_write: 0, started_ms: 4000, first_token_ms: None }));
        assert_eq!(h[1].time_ms, 5000);
    }
    #[test]
    fn session_stats_count_the_whole_conversation() {
        let dir = std::env::temp_dir().join(format!("heimdall-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let lines = [
            json!({"type":"session","id":"s","timestamp":"2026-10-14T20:24:54.000Z"}),
            json!({"type":"message","id":"u","timestamp":"2026-10-14T20:25:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
            json!({"type":"message","id":"a","timestamp":"2026-10-14T20:25:04.000Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"c","name":"edit"},{"type":"toolCall","id":"d","name":"bash"}],"usage":{"input":10,"output":20,"cacheRead":1000,"cacheWrite":50}}}),
            json!({"type":"message","id":"r","timestamp":"2026-10-14T20:25:05.000Z","message":{"role":"toolResult","toolName":"edit","isError":true,"content":[{"type":"text","text":"old_string not found"}]}}),
            json!({"type":"message","id":"r2","timestamp":"2026-10-14T20:25:05.000Z","message":{"role":"toolResult","toolName":"mcp__heimdall__speak","isError":true,"content":[]}}),
            json!({"type":"compaction","id":"k","timestamp":"2026-10-15T08:00:00.000Z","summary":"…"}),
            json!({"type":"message","id":"a2","timestamp":"2026-10-15T08:00:09.000Z","message":{"role":"assistant","stopReason":"error","errorMessage":"overloaded","content":[],"usage":{"input":5,"output":0,"cacheRead":0,"cacheWrite":0}}}),
        ];
        std::fs::write(&path, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let s = session_stats(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((s.requests, s.input, s.output, s.cache_read, s.cache_write), (2, 15, 20, 1000, 50));
        assert_eq!(s.last_prompt, 5);
        assert_eq!((s.tool_calls, s.tool_failures, s.failed_edits), (2, 2, 1));
        assert_eq!((s.compactions, s.errors), (1, 1));
        assert_eq!(s.since_ms, iso_ms("2026-10-14T20:24:54.000Z").unwrap());
        assert_eq!(s.through_ms, iso_ms("2026-10-15T08:00:09.000Z").unwrap());
        assert_eq!(s.last_compaction_ms, iso_ms("2026-10-15T08:00:00.000Z"));
        assert!(is_edit_tool("mcp__x__Edit") && !is_edit_tool("bash"));
    }
    #[test]
    fn typed_history_has_real_entry_id_and_attachment_metadata() {
        let mut v = LiveView::default();
        let e=v.history(&json!({"type":"message","id":"abc","message":{"role":"user","content":[{"type":"text","text":"[Heimdall typed message]\nhello"},{"type":"image","mimeType":"image/png"}]}}));
        assert_eq!(e[0].session_entry_id.as_deref(), Some("abc"));
        assert!(e[0].editable);
        assert_eq!(e[0].images[0].0, "image/png");
    }
    #[test]
    fn observation_images_and_unicode_are_preserved() {
        let mut v = SessionView::default();
        let e = v.sent(
            &crate::protocol::message(
                "id",
                "żółć",
                &[crate::protocol::Attachment {
                    media_type: "image/png".into(),
                    bytes: vec![1],
                }],
                false,
                false,
            )
            .unwrap(),
            5,
        );
        assert_eq!(e[0].text, "żółć\n[image]");
        assert_eq!(e[0].kind, Kind::Prompt { query: false });
        assert_eq!(iso_ms("1970-01-01T00:00:00Z"), Some(0));
    }
}

#[cfg(test)]
mod transcript_compatibility_tests {
    #[test]
    fn live_and_archived_prompts_decode_old_and_new_lines() {
        let old = "[Heimdall delivery: d]\nHeimdall captured transcript (labelled data):\nevent=session:1 timestamp_ms=1 source=\"Discord\" speaker=\"Ku\\\"ba\" lang=pl: cześć [screenshot 14:02:40 attached]";
        let expected = "Heimdall captured transcript (labelled data):\n[Discord · Ku\"ba] cześć [screenshot 14:02:40 attached]";
        assert_eq!(super::clean(old), expected);
        let wire = serde_json::json!({"type":"user","uuid":"d","message":{"content":[{"type":"text","text":old}]}});
        let mut view = super::LiveView::default();
        assert_eq!(view.sent(&wire, 1)[0].text, format!("[Heimdall delivery: d]\n{expected}"));
        let new = "Transcript:\n[Mic · User] hello";
        assert_eq!(super::clean(new), new);
        assert_eq!(super::clean("[Mic · User] I said event=bogus timestamp_ms=10"), "[Mic · User] I said event=bogus timestamp_ms=10");
    }

    /// Night pass 20→21 Oct (P1): the date line under the marker is for the model; the window shows what was said and
    /// a typed message stays editable.
    #[test]
    fn the_date_line_is_not_shown_and_typed_messages_stay_editable() {
        let at = 1_792_459_320_000; // 20 Oct 2026 evening
        let typed = crate::harness::marked_prompt("m", "[Heimdall typed message]\nhello there", at);
        assert!(typed.starts_with(&format!("[Heimdall delivery: m]\n{}\n", crate::clock::now_line(at))), "{typed}");
        assert_eq!(super::typed_text(&typed), Some("hello there"));
        assert_eq!(super::clean(&typed), "[Heimdall typed message]\nhello there");
        let wake = crate::harness::marked_prompt("d", "Transcript:\n[19:22:01 Mic · User] hello\n\nOwner's wake instruction:\nAnswer", at);
        let shown = super::message_text(&serde_json::json!([{"type":"text","text":wake}]));
        assert_eq!(shown, "[Heimdall delivery: d]\nTranscript:\n[19:22:01 Mic · User] hello\n\nOwner's wake instruction:\nAnswer");
        // Messages from before the date line read as before.
        assert_eq!(super::typed_text("[Heimdall delivery: m]\n[Heimdall typed message]\nhi"), Some("hi"));
    }
}

/// A failed turn's text: a provider's key, size or usage-limit error in words, else Pi's own message.
/// The row for a failed or stopped request. An aborted request is always Heimdall's own abort (the user's words steering
/// the turn, his Stop, Agent off: nobody else drives Pi), never a provider failure, whatever text the provider's client
/// gave it: Pi's OpenAI client calls a steer abort "OpenAI Responses stream ended before a terminal response event"
/// (audit 27 Oct, F4: four of them on 26-27 Oct, each within 100 ms of a wake, shown as "Turn failed").
fn failure_text(message: &Value) -> String {
    if message["stopReason"] == "aborted" { return "Turn stopped".into(); }
    let raw = message["errorMessage"].as_str().unwrap_or("Turn stopped");
    crate::provider::explain_error(message["provider"].as_str().unwrap_or_default(), raw).unwrap_or_else(|| raw.into())
}

/// Local wall-clock HH:MM for a Unix-millisecond instant.
fn clock_ms(ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms as i64).single().map_or_else(String::new, |t| t.format("%H:%M").to_string())
}
