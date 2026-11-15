//! One plain line in `heimdall.log` for every failure, fallback, retry, timeout and state change (
//! "whenever something happens in Heimdall, something doesn't work and stuff like that, we should be logging all of
//! that by default, so we have something to troubleshoot from"). The log sink puts the local time in front of every
//! line. Evidence records become lines at the archive's front door (`enqueued`, on the caller's thread), so a stuck or
//! full archive still leaves its trace here. Lines are single, bounded, and never carry keys or tokens.
use heimdall_archive::Record;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Write,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const LINE_LIMIT: usize = 700;
const FIELD_LIMIT: usize = 240;
const QUOTE_LIMIT: usize = 160;
/// The same line (numbers aside, for memory readings and health) is written once per window, then with its count.
const REPEAT_WINDOW: Duration = Duration::from_secs(600);

/// Writes `area: text` as one line to stderr (captured into `heimdall.log`).
pub(crate) fn line(area: &str, text: &str) {
    write(&format(area, text));
}
fn write(line: &str) {
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    // One write per line: concurrent writers never split each other's lines.
    let _ = std::io::stderr().lock().write_all(&bytes);
}
/// `area: text`, folded onto one line, secrets masked, bounded.
pub(crate) fn format(area: &str, text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{area}: {}", bounded(&redact(&flat), LINE_LIMIT))
}
/// The archive hook: every evidence record that is an event (not a `QUIET` stream) becomes one line.
pub(crate) fn enqueued(r: &Record) {
    if let Some((line, repeats)) = evidence(r) {
        let line = match repeats {
            Repeats::Kept => Some(line),
            Repeats::Folded => admit(&line, false, Instant::now()),
        };
        if let Some(line) = line {
            write(&line);
        }
    }
}
/// Whether a line that recurs (the same apart from its numbers) within `REPEAT_WINDOW` is folded into a count: yes for
/// a state that loops (a memory refusal every 30 s, a part flapping), never for distinct events (a voice switch, a
/// wake, the agent's state, a rejected take).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeats {
    Kept,
    Folded,
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
/// Masks what could be a credential: `sk-…` keys, bearer tokens, the value after a key/token/secret/password name,
/// and long base64/hex runs (keys, hashes, encoded payloads).
pub(crate) fn redact(text: &str) -> String {
    const NAMES: [&str; 7] = ["api_key", "apikey", "api-key", "token", "secret", "password", "authorization"];
    let mut out = Vec::new();
    let mut mask_next = false;
    for word in text.split(' ') {
        let lower = word.to_ascii_lowercase();
        let bare = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        let masked = if mask_next && bare.len() >= 6 {
            "…".to_owned()
        } else if let Some(at) = lower.find("sk-").filter(|at| word[at + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').count() >= 8) {
            format!("{}sk-…", &word[..at])
        } else if let Some(name_end) = NAMES.iter().find_map(|name| {
            // `name=value` or `"name":value` within this word; the separator's index.
            let at = lower.find(name)?;
            let rest = &lower[at + name.len()..];
            let sep = rest.find(|c: char| c == '=' || c == ':')?;
            rest[..sep].chars().all(|c| c == '"' || c == '\'' || c == '_').then_some(at + name.len() + sep)
        }) {
            let value = word[name_end + 1..].trim_matches(|c: char| c == '"' || c == '\'');
            if value.len() >= 6 { format!("{}…", &word[..name_end + 1]) } else { word.to_owned() }
        } else {
            blobs(word)
        };
        // "Bearer <token>" and `"api_key": "<value>"` put the secret in the next word.
        mask_next = bare == "bearer"
            || NAMES.iter().any(|name| lower.ends_with(&format!("{name}:")) || lower.ends_with(&format!("{name}\":")) || lower.ends_with(&format!("{name}=")));
        out.push(masked);
    }
    out.join(" ")
}
/// Long runs of base64/hex characters (40+, no '-' or '_' so file names and ids stay readable) become `…`.
fn blobs(word: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 40 { out.push('…') } else { out.push_str(run) }
        run.clear();
    };
    for c in word.chars() {
        if c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}
/// Whether a line is written now: the same line (with `numbers_matter` false, the same apart from its numbers) is
/// written once per `REPEAT_WINDOW`, and the next one after the window says how many were left out.
fn admit(line: &str, numbers_matter: bool, now: Instant) -> Option<String> {
    static SEEN: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();
    let key: String = if numbers_matter { line.to_owned() } else { line.chars().filter(|c| !c.is_ascii_digit()).collect() };
    let Ok(mut seen) = SEEN.get_or_init(Default::default).try_lock() else { return Some(line.to_owned()) };
    if seen.len() > 512 {
        seen.retain(|_, (at, _)| now.duration_since(*at) < REPEAT_WINDOW);
        if seen.len() > 512 {
            seen.clear();
        }
    }
    match seen.get_mut(&key) {
        Some((at, skipped)) if now.duration_since(*at) < REPEAT_WINDOW => {
            *skipped += 1;
            None
        }
        Some((at, skipped)) => {
            let line = if *skipped > 0 { format!("{line} (and {skipped} more like it in the last {} min)", REPEAT_WINDOW.as_secs() / 60) } else { line.to_owned() };
            *at = now;
            *skipped = 0;
            Some(line)
        }
        None => {
            seen.insert(key, (now, 0));
            Some(line.to_owned())
        }
    }
}

/// A value as plain text: strings as written, numbers and booleans as JSON, nothing for null or empty.
fn plain(v: &Value) -> Option<String> {
    let text = match v {
        Value::Null => return None,
        Value::String(s) => s.trim().to_owned(),
        Value::Array(a) if a.is_empty() => return None,
        Value::Object(o) if o.is_empty() => return None,
        other => other.to_string(),
    };
    (!text.is_empty()).then(|| bounded(&text, FIELD_LIMIT))
}
fn quoted(v: &Value) -> String {
    format!("\"{}\"", bounded(v.as_str().unwrap_or(""), QUOTE_LIMIT))
}
fn gb(v: &Value) -> Option<String> {
    v.as_f64().map(|b| format!("{:.1} GB", b / 1e9))
}
/// The line for a record worth one, and whether its repeats fold.
pub(crate) fn evidence(r: &Record) -> Option<(String, Repeats)> {
    use Repeats::{Folded, Kept};
    let p = &r.payload;
    let kind = r.kind.as_str();
    let (area, text, repeats) = match kind {
        "health" => {
            let (area, text) = health(p)?;
            (area, text, Folded)
        }
        "drama3" => {
            let (text, repeats) = drama3(p)?;
            ("voice", text, repeats)
        }
        "agent_state" => ("agent", agent_state(&r.source, p)?, Kept),
        "send_decision" => ("decider", decision(r)?, Kept),
        "settings_applied" | "timeline_settings" => ("settings", switches(p)?, Kept),
        "barge_in" => ("speech", barge_in(p)?, Kept),
        // A held or skipped line is an event of its own (bug note 2026-11-13T13-55-33): never folded into a count.
        "speech_held" | "speech_skipped" => ("speech", generic(r), Kept),
        "tts_error" => ("speech", format!("lighter voice failed: {} (sentence {})", plain(&p["error"]).unwrap_or_default(), quoted(&p["text"])), Kept),
        "invariant_violation" => ("check", invariant(p)?, Folded),
        "listening_ready" => ("listen", format!("models ready {} ms after Listening on", p["ms_since_on"].as_f64().map_or("?".into(), |ms| format!("{ms:.0}"))), Kept),
        "capture_gap" | "capture_stalled" | "capture_discontinuity" | "capture_diagnostic" | "gap" | "source_removed" | "source_readded" | "microphone_cue" => ("capture", generic(r), Folded),
        "relaunched" | "recovery" | "archive_stopped" | "diagnostic_drops" => ("heimdall", generic(r), Folded),
        // One plain line per conversation repair step (`agent::history`).
        "agent_history_repair" => ("agent", plain(&p["line"])?, Kept),
        "agent_start_failed" | "agent_runtime_ended" | "agent_queue_fallback" | "agent_loop_guard" | "agent_reload" | "agent_reload_stale" | "agent_idle_compaction_failed" | "agent_preflight" | "agent_stop_wait" => ("agent", generic(r), Kept),
        "command_action" if heimdall_archive::diagnostics::problem(kind, p) => ("command", generic(r), Kept),
        // ---- every event (bug note 2026-11-13T13-55-33): an event's own line, a failure included ("delivery … refused") ----
        _ => match (event(r), heimdall_archive::diagnostics::problem(kind, p)) {
            // A failing event keeps its error even when its own line has no place for it.
            (Some(Some((area, text))), problem) => match plain(&p["error"]).filter(|e| problem && !text.contains(e.as_str())) {
                Some(e) => (area, format!("{text}; error: {e}"), Kept),
                None => (area, text, Kept),
            },
            (_, true) => (area_of(kind), generic(r), Folded),
            // A known kind whose record is not an event (an island hold), or a quiet kind.
            (Some(None), false) => return None,
            (None, false) if QUIET.contains(&kind) => return None,
            // A kind with no line of its own yet: its kind, source and the fields that say what happened.
            (None, false) => (area_of(kind), brief(r), Folded),
        },
        // ---- end every event ----
    };
    Some((format(area, &text), repeats))
}
// ---- every event (bug note 2026-11-13T13-55-33) ----
// "on the Mac we should be also logging every single event or whatever is happening so that
// debugging is easier". Every record kind now gets one line unless it is listed here: streams and per-frame telemetry
// (hundreds to millions a day) and records whose payload is message text (transcripts, the agent's wire traffic and
// context). Each of these has an event line elsewhere (an utterance's decision, the agent's state, a delivery) or is
// read from the archive (`heimdall-archive timeline`). The audit: lab/event-log-audit-2026-11-13.md.
pub(crate) const QUIET: &[&str] = &[
    // Per frame or per audio chunk.
    "model_output", "night_model_output", "tts_model_output", "speaker_probabilities", "speaker_segments_delta",
    "speaker_segments_finalized", "speaker_embeddings", "listen_event", "vad", "speech_audio",
    "speech_split_call_cpu", "speech_split_progress", "speech_split_asr_output", "speech_split_admission",
    "speech_split_completed", "speech_split_pending",
    // Per utterance, carrying its words or its scoring; the wake, the command and the delivery have lines.
    "utterance", "utterance_binding", "transcript_segment", "transcript_revision", "mic_voice", "suggest_decision",
    "command_decision", "send_context", "user_message", "typed",
    // The agent's wire traffic and context (message bodies); deliveries and the agent's state have lines.
    "agent_transport_send", "agent_reply", "agent_context",
    "agent_context_routing", "agent_thinking",
    // The island's copy of a health cue (the `health` record has the line).
    "health_cue",
];
/// The line for an event record: `Some(None)` for a known kind whose record is not an event, `None` for a kind with no
/// line of its own (it gets `brief`).
fn event(r: &Record) -> Option<Option<(&'static str, String)>> {
    let p = &r.payload;
    let yes = |b: &Value| b.as_bool() == Some(true);
    let ms = |v: &Value| v.as_f64().map(|ms| format!(" after {ms:.0} ms")).unwrap_or_default();
    let why = |v: &Value| plain(v).map(|w| format!(" ({w})")).unwrap_or_default();
    let n = |v: &Value| v.as_u64().or_else(|| v.as_array().map(|a| a.len() as u64)).unwrap_or(0);
    let images = |v: &Value| match n(v) { 0 => String::new(), 1 => ", 1 image".into(), k => format!(", {k} images") };
    let line = |area: &'static str, text: String| Some(Some((area, text)));
    match r.kind.as_str() {
        // Deliveries to the agent and their acknowledgement.
        "agent_delivery" => {
            let id = plain(&p["message_id"]).unwrap_or_else(|| "?".into());
            match plain(&p["error"]) {
                Some(e) => line("agent", format!("delivery {id} refused: {e}")),
                None if yes(&p["transport_accepted"]) => line("agent", format!("delivery {id} accepted by the agent")),
                None => line("agent", format!("delivery {id} not accepted")),
            }
        }
        "agent_delivery_resolved" => line("agent", format!("delivery {} resolved: {}{}", plain(&p["id"]).unwrap_or_else(|| "?".into()),
            plain(&p["outcome"]).unwrap_or_else(|| "?".into()), why(&p["reason"]))),
        "send_decision_delivery" => line("agent", format!("decided line delivered{}", why(&p["outcome"]))),
        "agent_wake" => line("agent", format!("woke{}{}", if yes(&p["urgent"]) { " (urgent)" } else { "" }, images(&p["image_ids"]))),
        "agent_steer" => line("agent", format!("steered the running turn ({}{})", if yes(&p["delivered"]) { "delivered" } else { "not delivered" }, images(&p["image_ids"]))),
        "notch_drop" => line("notch", format!("drop received: {} {}{}{}", n(&p["items"]), plain(&p["kind"]).unwrap_or_else(|| "items".into()),
            plain(&p["label"]).map(|l| format!(" ({})", bounded(&l, 80))).unwrap_or_default(),
            if yes(&p["composer"]) { " in the composer" } else { "" })),
        "notch_drop_delivery" => line("notch", format!("drop delivered to the agent ({}{}{})", plain(&p["label"]).map(|l| bounded(&l, 80)).unwrap_or_else(|| "drop".into()),
            images(&p["images"]), if yes(&p["joined"]) { ", joined to an utterance" } else { "" })),
        // Speech: the agent's lines, acknowledgements and their playback. Word counts, never the words.
        "speak_back" => {
            let words = p["text"].as_str().map_or(0, |t| t.split_whitespace().count());
            let off = if p["enabled"] == false { "; Speaking is off, not spoken" } else { "" };
            line("speech", format!("the agent asked to speak {words} words ({}){off}", plain(&p["language"]).unwrap_or_else(|| "?".into())))
        }
        "voice_ack" => line("speech", match plain(&p["outcome"]) {
            Some(o) => format!("acknowledgement {o}{}", plain(&p["decider_line"]).map(|l| format!(" (line {l})")).unwrap_or_default()),
            None => format!("no acknowledgement{}", why(&p["reason"])),
        }),
        "voice_ack_playback" | "voice_reply_playback" => {
            let what = if r.kind == "voice_ack_playback" { "acknowledgement" } else { "voice-bank reply" };
            let clip = plain(&p["clip_id"]).map(|c| format!(" {c}")).unwrap_or_default();
            let outcome = plain(&p["outcome"]).unwrap_or_else(|| "?".into());
            let error = plain(&p["error"]).map(|e| format!(": {e}")).unwrap_or_default();
            line("speech", format!("{what}{clip} {outcome}{}{error}{}", why(&p["reason"]), ms(&p["elapsed_ms"])))
        }
        "speech_stopped" => line("speech", format!("stopped{}{}", why(&p["reason"]), plain(&p["speaker"]).map(|s| format!(" by {s}")).unwrap_or_default())),
        "voice_reply" => line("speech", match (plain(&p["error"]).or_else(|| plain(&p["audio_error"])), plain(&p["outcome"])) {
            (Some(e), _) => format!("voice reply failed: {e}"),
            (None, o) => format!("voice reply: {}{}", o.unwrap_or_else(|| "?".into()), why(&p["reason"])),
        }),
        "echo_guard" => line("listen", format!("echo guard: {}", plain(&p["outcome"]).unwrap_or_else(|| "?".into()))),
        // The user's keys and the island.
        "hotkey" => line("hotkey", match p["key"].as_str() {
            Some("wake") if yes(&p["woke"]) => format!("wake pressed: woke the agent with {} line(s)", n(&p["lines"])),
            Some("wake") => "wake pressed: nothing sent (the agent is off or stopped)".into(),
            Some("stop") => format!("stop pressed{}", if yes(&p["speaking"]) { " while Heimdall spoke" } else { "" }),
            Some(key) => format!("{key} pressed"),
            None => "a key pressed".into(),
        }),
        "island_report" => {
            let kind = plain(&p["kind"]).unwrap_or_else(|| "?".into());
            let label = plain(&p["label"]).map(|l| format!(" \"{}\"", bounded(&l, 80))).unwrap_or_default();
            line("island", match p["event"].as_str() {
                Some("shown") => format!("opened {kind}{label}{}", ms(&p["waited_ms"])),
                Some("cut") => format!("cut {kind}{label} after {} ms{}", n(&p["shown_ms"]), why(&p["why"])),
                Some("left") => format!("closed {kind} after {} ms{}", n(&p["shown_ms"]), why(&p["why"])),
                _ => format!("{kind}{label}"),
            })
        }
        "island_cue" => match p["cue"].as_str() {
            Some("agent_sent") => line("island", "sent to the agent".into()),
            Some("vella") => line("island", format!("Vella dictation {}", if yes(&p["on"]) { "on" } else { "off" })),
            Some("agent_notice") => line("island", format!("agent notice ({}): \"{}\"", plain(&p["kind"]).unwrap_or_else(|| "?".into()),
                bounded(p["text"].as_str().unwrap_or(""), 80))),
            _ => Some(None),
        },
        "command_cue" => line("command", format!("cue {} #{}", plain(&p["phase"]).unwrap_or_else(|| "?".into()), plain(&p["id"]).unwrap_or_else(|| "?".into()))),
        "command_execution" => line("command", format!("voice command #{} ran: {}", plain(&p["cue_id"]).unwrap_or_else(|| "?".into()),
            bounded(&plain(&p["command"]).unwrap_or_default(), 80))),
        "screen_command" => line("command", format!("screen command{}", if yes(&p["screen_off"]) { ": Seeing is off, nothing captured" } else { "" })),
        // State: Listening, the people Heimdall listens to, sources and devices.
        "heimdall_on" | "heimdall_off" => line("listen", format!("Listening {}{}", if r.kind == "heimdall_on" { "on" } else { "off" }, why(&p["reason"]))),
        "listening_changed" => line("listen", format!("listening to {} {}{}", plain(&p["speaker"]).unwrap_or_else(|| "?".into()),
            if yes(&p["on"]) { "on" } else { "off" }, plain(&p["by"]).map(|b| format!(" (by {b})")).unwrap_or_default())),
        // `kind` is `"Microphone"` or `{"InputDevice":{…}}` / `{"Process":{…}}` (capture::SourceKind).
        "source" => line("capture", format!("source added: {} \"{}\" (generation {})",
            p["kind"].as_str().map(str::to_owned).or_else(|| p["kind"].as_object().and_then(|o| o.keys().next().cloned())).unwrap_or_else(|| "?".into()),
            bounded(p["label"].as_str().unwrap_or(""), 80), n(&p["generation"]))),
        // A turn handed to the agent's process and its acknowledgement (relay and Pi alike); context reads, stats
        // polls and the streamed message parts stay out (`wire` carries message text: never quoted).
        "agent_transport_sent" => match p["wire"]["type"].as_str() {
            Some(t @ ("prompt" | "steer" | "follow_up" | "abort")) => line("agent", format!("sent {t} {}", short_id(&p["wire"]["id"]))),
            _ => Some(None),
        },
        "agent_transport_reply" => match (p["wire"]["type"].as_str(), p["wire"]["command"].as_str()) {
            (Some("response"), Some(c @ ("prompt" | "steer" | "follow_up" | "abort"))) => line("agent", match p["wire"]["success"].as_bool() {
                Some(true) => format!("{c} {} acknowledged{}", short_id(&p["wire"]["id"]), plain(&p["wire"]["data"]["disposition"]).map(|d| format!(" ({d})")).unwrap_or_default()),
                _ => format!("{c} {} refused: {}", short_id(&p["wire"]["id"]), plain(&p["wire"]["error"]).unwrap_or_else(|| "no reason given".into())),
            }),
            (Some("response"), _) if p["wire"]["success"] == false => line("agent", format!("{} {} refused: {}",
                plain(&p["wire"]["command"]).unwrap_or_else(|| "request".into()), short_id(&p["wire"]["id"]), plain(&p["wire"]["error"]).unwrap_or_else(|| "no reason given".into()))),
            _ => Some(None),
        },
        "vella_dictation" => line("vella", match (&p["notice"], &p["expired"]) {
            (Value::Object(_), _) => format!("dictation notice: {}", plain(&p["notice"]["reason"]).unwrap_or_else(|| "?".into())),
            (_, Value::Null) => "a malformed notice, ignored".into(),
            (_, by) => format!("dictation ended ({})", plain(by).unwrap_or_else(|| "expired".into())),
        }),
        "agent_look" => line("agent", format!("looked at display {} ({}×{})", n(&p["display"]), n(&p["width"]), n(&p["height"]))),
        _ => None,
    }
}
/// A wire id as far as its first group: "control-4808440b".
fn short_id(v: &Value) -> String {
    let id = v.as_str().unwrap_or("?");
    let end = id.match_indices('-').nth(1).map_or(id.len(), |(at, _)| at);
    bounded(&id[..end], 40)
}
/// `kind (source): key: value; …` from the fields that say what happened, never free text (bodies, transcripts,
/// prompts) or nested objects: the catch-all for a kind with no line of its own.
fn brief(r: &Record) -> String {
    const KEYS: [&str; 18] = ["event", "state", "outcome", "status", "phase", "stage", "component", "check", "command", "key", "on",
        "by", "reason", "cause", "error", "exit_code", "count", "ms"];
    let p = &r.payload;
    let parts: Vec<String> = KEYS.iter().filter_map(|k| match &p[*k] {
        v @ (Value::Bool(_) | Value::Number(_)) => Some(format!("{k}: {v}")),
        Value::String(s) if !s.trim().is_empty() => Some(format!("{k}: {}", bounded(s.trim(), 120))),
        _ => None,
    }).collect();
    let source = (!r.source.is_empty() && r.source != "heimdall").then(|| format!(" ({})", bounded(&r.source, 60))).unwrap_or_default();
    format!("{}{source}{}", r.kind, if parts.is_empty() { String::new() } else { format!(": {}", parts.join("; ")) })
}
/// One line per action the user takes, with where it came from: `menu` (the Mac's menu, island and windows), `phone`
/// (the phone's settings and cards), `agent tool` (the agent's `settings` tool). Typed text, secrets,
/// paths and names are left out: a variant with a string or a list in it is named only by its variant path.
pub(crate) fn action(source: &str, debug: &str) -> Option<String> {
    Some(format("action", &format!("{source}: {}", action_name(debug)?)))
}
/// Writes `action`'s line.
pub(crate) fn write_action(source: &str, debug: &str) {
    if let Some(line) = action(source, debug) {
        write(&line);
    }
}
fn action_name(debug: &str) -> Option<String> {
    // Telemetry, not an action: the island's own shown/closed reports have their `island_report` line.
    if debug.starts_with("Island(") {
        return None;
    }
    if !debug.contains('"') && debug.len() <= 160 {
        return Some(debug.to_owned());
    }
    let head = &debug[..debug.find(['"', '[']).unwrap_or(debug.len())];
    let path: Vec<&str> = head.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| w.chars().next().is_some_and(|c| c.is_ascii_uppercase())).collect();
    Some(format!("{} (details withheld)", path.join(" ")))
}
// ---- end every event ----
/// A failed invariant with its scalar evidence (`queue_depth=2, unresolved=3 items`). The `drama3_problem` and
/// `send_decision_problem` checks repeat a record that already has its own line.
fn invariant(p: &Value) -> Option<String> {
    let check = plain(&p["check"])?;
    if check.starts_with("drama3_problem") || check.starts_with("send_decision_problem") {
        return None;
    }
    let evidence: Vec<String> = p["evidence"].as_object().into_iter().flatten()
        .filter(|(k, _)| !k.ends_with("evidence_id") && !k.ends_with("evidence_ids"))
        .filter_map(|(k, v)| match v {
            Value::Array(items) if !items.is_empty() => Some(format!("{k}={} items", items.len())),
            Value::Object(_) | Value::Array(_) | Value::Null => None,
            other => plain(other).map(|v| format!("{k}={v}")),
        })
        .collect();
    let extra: Vec<String> = p.as_object().into_iter().flatten()
        .filter(|(k, v)| !["check", "evidence"].contains(&k.as_str()) && !k.ends_with("evidence_id") && !k.ends_with("evidence_ids") && !v.is_object() && !v.is_array())
        .filter_map(|(k, v)| plain(v).map(|v| format!("{k}={v}")))
        .collect();
    let all: Vec<String> = evidence.into_iter().chain(extra).collect();
    Some(if all.is_empty() { check } else { format!("{check}: {}", all.join(", ")) })
}
/// The innermost `"reason":"…"` of a reason that wraps a worker's JSON line ("Drama 3: {\"event\":\"fatal\",…}").
fn inner_reason(reason: &str) -> String {
    let Some(start) = reason.rfind("\"reason\":\"").map(|at| at + 10) else { return reason.to_owned() };
    let mut out = String::new();
    let mut escaped = false;
    for c in reason[start..].chars() {
        match (escaped, c) {
            (false, '\\') => escaped = true,
            (false, '"') => return out,
            (_, c) => {
                out.push(c);
                escaped = false;
            }
        }
    }
    reason.to_owned()
}
fn area_of(kind: &str) -> &'static str {
    match kind.split('_').next().unwrap_or("") {
        "agent" => "agent",
        "listen" | "utterance" | "transcript" => "listen",
        "capture" | "gap" => "capture",
        "tts" | "voice" | "speech" => "speech",
        "decision" | "d1" | "decider" | "command" => "decider",
        "speaker" | "speakers" => "speakers",
        "t3" => "t3",
        _ => "heimdall",
    }
}
/// `kind (source): key: value; …` from the fields that say what happened, then error fields one level down.
fn generic(r: &Record) -> String {
    const KEYS: [&str; 18] = ["event", "state", "component", "phase", "stage", "check", "outcome", "status", "command", "reason", "error", "audio_error", "problem", "detail", "message", "notice", "label", "exit_code"];
    let p = &r.payload;
    let mut parts: Vec<String> = KEYS.iter().filter_map(|k| plain(&p[*k]).map(|v| format!("{k}: {v}"))).collect();
    if let Value::Object(fields) = p {
        for (name, nested) in fields.iter().filter(|(k, v)| ["decision", "result", "output", "wire"].contains(&k.as_str()) && v.is_object()) {
            for (k, v) in nested.as_object().into_iter().flatten() {
                if k.ends_with("error") || k == "reason" {
                    if let Some(v) = plain(v) {
                        parts.push(format!("{name}.{k}: {v}"));
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        parts = p.as_object().into_iter().flatten().filter(|(k, v)| k.as_str() != "system_context" && !v.is_object() && !v.is_array())
            .take(4).filter_map(|(k, v)| plain(v).map(|v| format!("{k}: {v}"))).collect();
    }
    let source = (!r.source.is_empty() && r.source != "heimdall").then(|| format!(" ({})", bounded(&r.source, 60))).unwrap_or_default();
    format!("{}{source}: {}", r.kind, if parts.is_empty() { "recorded".into() } else { parts.join("; ") })
}
/// Component status (`reason`, `previous`) and island health cues (`cue`).
fn health(p: &Value) -> Option<(&'static str, String)> {
    let component = plain(&p["component"]).unwrap_or_else(|| "?".into());
    let state = plain(&p["state"]).unwrap_or_default();
    if let Some(cue) = p["cue"].as_str() {
        let label = cue_label(cue);
        let reason = plain(&p["reason"]).map(|r| format!(" ({r})")).unwrap_or_default();
        return Some(("island", match label {
            Some(label) if component == "notice" => format!("notice: {label}{reason}"),
            Some(label) => format!("{component} {state}: {label}{reason}"),
            None => format!("{component} {state}{reason}"),
        }));
    }
    Some(("health", match (state.as_str(), plain(&p["reason"])) {
        ("recovered", _) => match plain(&p["previous"]) {
            Some(previous) => format!("{component} recovered (was: {previous})"),
            None => format!("{component} recovered"),
        },
        (_, Some(reason)) => format!("{component} {state}: {reason}"),
        _ => format!("{component} {state}"),
    }))
}
/// The `label: "…"` of a cue's debug text.
fn cue_label(cue: &str) -> Option<String> {
    let start = cue.find("label: \"")? + 8;
    let mut out = String::new();
    let mut escaped = false;
    for c in cue[start..].chars() {
        match (escaped, c) {
            (false, '\\') => escaped = true,
            (false, '"') => return Some(out),
            (_, c) => {
                out.push(c);
                escaped = false;
            }
        }
    }
    None
}
/// A word check's wrong words: "cache→cash", "-word" dropped, "+word" added.
fn edit_list(result: &Value) -> Vec<String> {
    result["edits"].as_array().into_iter().flatten().map(|e| match e["kind"].as_str() {
        Some("substitution") => format!("{}→{}", e["expected"].as_str().unwrap_or(""), e["actual"].as_str().unwrap_or("")),
        Some("deletion") => format!("-{}", e["expected"].as_str().unwrap_or("")),
        _ => format!("+{}", e["actual"].as_str().unwrap_or("")),
    }).collect()
}
/// Jarvis's load, unload, refusals, retries and every switch to the lighter voice, with the reason and the sentence.
fn drama3(p: &Value) -> Option<(String, Repeats)> {
    use Repeats::{Folded, Kept};
    let reason = plain(&p["reason"]).map(|r| inner_reason(&r)).unwrap_or_else(|| "no reason given".into());
    let free = gb(&p["available_bytes"]).map(|g| format!(" ({g} available)")).unwrap_or_default();
    let phase = match p["phase"].as_str() {
        Some("load") => "loading",
        Some("speech") => "speaking",
        Some(other) => other,
        None => "?",
    };
    Some(match p["event"].as_str()? {
        "ready" => {
            let engine = match p["engine"].as_str() { Some("rust") => " (Rust engine)", Some("python") => " (Python worker)", _ => "" };
            (format!("Jarvis ready{engine}, loaded in {:.1} s", p["load_ms"].as_f64().unwrap_or(0.) / 1000.), Folded)
        }
        "engine_fallback" => (format!("Jarvis's Rust engine failed to start, the Python worker takes over until restart: {reason}"), Kept),
        // One greppable event per switch, its fields first so the line bound never cuts them (docs/personal/nightly-build.md).
        "voice_switch" => {
            let error = plain(&p["error"]).map(|e| inner_reason(&e)).unwrap_or_else(|| reason.clone());
            let cause = p["cause"].as_str().map(str::to_owned).unwrap_or_else(|| speak::drama3::fallback_cause(&error).into());
            let why = if error == reason { String::new() } else { format!(" — {reason}") };
            (format!("fallback cause={cause} from={} to={} error={} · {}: the lighter voice says {}{why}",
                p["from"].as_str().unwrap_or("jarvis"), p["to"].as_str().unwrap_or("lighter"), quoted(&Value::from(error.as_str())),
                plain(&p["notice"]).unwrap_or_else(|| "voice switch".into()), quoted(&p["text"])), Kept)
        }
        "batch" => (format!("Jarvis batch: sentences {} to {} together, {:.1} s of audio ahead", p["first"].as_u64().unwrap_or(0) + 1,
            p["first"].as_u64().unwrap_or(0) + p["rows"].as_u64().unwrap_or(0), p["lead_s"].as_f64().unwrap_or(0.)), Folded),
        "load_retry" => (format!("Jarvis load failed, trying once more: {reason}"), Kept),
        "sentence_coverage" if p["result"]["kept_after_retries"] == true => {
            (format!("Jarvis take kept (sentence {}, attempt {} of {}): {}, his best take rather than the lighter voice; heard {}",
                p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS, edit_list(&p["result"]).join(", "),
                quoted(&p["result"]["transcript"])), Kept)
        }
        "take_kept" => (format!("Jarvis take kept (sentence {}, attempt {} of {}): his best take rather than the lighter voice after {}",
            p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS, plain(&p["reason"]).unwrap_or_else(|| "his retries".into())), Kept),
        "interrupted_restart" => (format!("Jarvis reloading at once: {reason} after an interruption (not counted as a failure)"), Folded),
        "sentence_coverage" if p["result"]["accepted"] == true => {
            // A take kept because an earlier take was rejected for the same lone word (speak::coverage).
            let spellings = || p["result"]["spellings"].as_array().into_iter().flatten();
            // Or for the same wrong words (`coverage::accept_agreed_take`).
            if let Some(agreed) = spellings().find(|s| s["kind"] == "agreed") {
                return Some((format!("Jarvis take kept (sentence {}, attempt {} of {}): {} heard alike on two takes, the recognizer's spelling",
                    p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS,
                    edit_list(&json!({"edits": agreed["edits"]})).join(", ")), Kept));
            }
            let repeated = spellings().find(|s| s["kind"] == "repeated")?;
            (format!("Jarvis take kept (sentence {}, attempt {} of {}): {}→{} misheard on two takes, the recognizer's spelling",
                p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS,
                repeated["expected"].as_str().unwrap_or(""), repeated["actual"].as_str().unwrap_or("")), Kept)
        }
        "sentence_coverage" => {
            let edits = edit_list(&p["result"]);
            let why = if let Some(r) = plain(&p["result"]["reason"]) { r } else if edits.is_empty() { "cut or repeated".into() } else { edits.join(", ") };
            (format!("Jarvis take rejected by the word check (sentence {}, attempt {} of {}): {why}; heard {}",
                p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS, quoted(&p["result"]["transcript"])), Kept)
        }
        "stall" if p["reason"] != "word coverage" => (format!("Jarvis take failed (sentence {}, attempt {} of {}): {reason}", p["sentence"], p["attempt"].as_u64().map_or(0, |a| a + 1), speak::drama3::ATTEMPTS), Kept),
        "restart_first" => ("Jarvis restarts his first sentence (the live take stalled)".into(), Kept),
        // Each carries its cause class, so the night pass's sweep (`cause=`, docs/personal/nightly-build.md 1a) finds a drop that
        // wrote no fallback line (9 Nov 12:38: a refused reload left only "Jarvis stopped: Need 55 GB …").
        "fatal" => (format!("Jarvis stopped: {reason}{free} [cause={}]", speak::drama3::fallback_cause(&reason)), Folded),
        "memory_refusal" => (format!("Jarvis not loaded: {reason} [cause=memory]"), Folded),
        "previous_worker_reaped" => (format!("Jarvis's reload waited {:.1} s for the stopped worker to free its memory",
            p["waited_ms"].as_f64().unwrap_or(0.) / 1000.), Folded),
        "previous_worker_lingering" => (format!("Jarvis's reload found a stopped worker still holding memory after {:.1} s; loading anyway",
            p["waited_ms"].as_f64().unwrap_or(0.) / 1000.), Kept),
        // At load the refusal or fatal line just before says why.
        "memory_unload" if phase == "loading" && p["reason"].is_null() => return None,
        // The memory line (bug note 2026-11-03T22-38-44): the give-way line says why; the unload just after is quiet.
        // Every give-way, hold and return names its reason, the need files and the hold (bug note 2026-11-08T02-03-45).
        "memory_give_way" => (format!("Jarvis gives way to Phonon S: {reason}{}; {}", needed_by(p), hold_after(p)), Kept),
        "memory_hold" => (format!("Jarvis held out for memory: {reason}{}; {}", needed_by(p), hold_after(p)), Kept),
        "memory_unload" if phase == "give_way" => return None,
        // The same GB (GiB) as the give-way reason (`memory_line::gb`), never this file's decimal `gb`.
        "memory_return" => (format!("Jarvis reloading: memory back under the {} GB line with room for him ({} GB in use){}{}",
            speak::memory_line::line_gb(), speak::memory_line::gb(p["used_bytes"].as_u64().unwrap_or(0)),
            p["out_s"].as_f64().map(|s| format!("; out {}", span(s))).unwrap_or_default(),
            match (p["released"].as_bool() == Some(true), p["hold_s"].as_f64()) {
                (true, Some(h)) => format!(", the need released and a {} hold waited out", span(h)),
                (false, Some(h)) => format!(" after a {} hold", span(h)),
                _ => String::new(),
            }), Kept),
        // A switch, not the memory line (bug note 2026-11-08T02-03-45: 01:38:01 read as a give-way with no reason).
        "unwanted" => {
            let held = plain(&p["held"]).map(|h| format!("; still held for memory: {h}")).unwrap_or_default();
            let what = if p["resident"] == true { "Jarvis unloaded" } else { "Jarvis no longer wanted" };
            (format!("{what}: {reason} (a switch, not the memory line{}){held}", in_use(p)), Kept)
        }
        "wanted" => {
            let held = plain(&p["held"]).map(|h| format!("; held for memory until it is back: {h}")).unwrap_or_else(|| "; loading".into());
            (format!("Jarvis wanted: {reason}{}{held}", in_use(p)), Kept)
        }
        "memory_line" => return None,
        "memory_unload" => (format!("Jarvis exited for memory (exit 73, while {phase}): {reason}"), Folded),
        "failure" => (format!("Jarvis failed: {reason} [cause={}]", speak::drama3::fallback_cause(&reason)), Folded),
        "unloaded" => ("Jarvis unloaded (no longer wanted)".into(), Folded),
        "verifier_unload_pending" | "verifier_reserve_carried" => (format!("word-check decoder still running; {} kept reserved", gb(&p["retained_bytes"]).unwrap_or_default()), Folded),
        _ => return None,
    })
}
/// " [needed by Vella Nemotron (angel-nemotron-wer.json)]" from a memory event's declared needs.
fn needed_by(p: &Value) -> String {
    let by = p["needed_by"].as_array().map(|w| w.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    if by.is_empty() { String::new() } else { format!(" [needed by {by}]") }
}
/// How long memory must stay back before he returns, from the event's `hold_s`.
fn hold_after(p: &Value) -> String {
    match p["hold_s"].as_f64() {
        Some(h) => format!("he returns once memory stays back for {}", span(h)),
        None => "he returns once memory is back".into(),
    }
}
/// "; 46.0 GB in use" in GB as the memory line counts them, when the event carries a reading.
fn in_use(p: &Value) -> String {
    p["used_bytes"].as_u64().filter(|b| *b > 0).map(|b| format!("; {} GB in use", speak::memory_line::gb(b))).unwrap_or_default()
}
/// A duration as a person says it: "45 s", "2 min", "6 min 12 s".
fn span(seconds: f64) -> String {
    let s = seconds.max(0.).round() as u64;
    match (s / 60, s % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}
fn agent_state(source: &str, p: &Value) -> Option<String> {
    static LAST: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let state = plain(&p["state"])?;
    let problem = plain(&p["problem"]);
    let line = match &problem {
        Some(problem) => format!("{state} — {problem}"),
        None => state,
    };
    if let Ok(mut last) = LAST.get_or_init(Default::default).try_lock() {
        if last.get(source) == Some(&line) {
            return None;
        }
        last.insert(source.into(), line.clone());
    }
    let queued = p["queue_depth"].as_u64().filter(|n| *n > 1).map(|n| format!(" ({n} queued)")).unwrap_or_default();
    Some(format!("{line}{queued}"))
}
/// Wakes and failures only: a "not for Heimdall" verdict is in the archive, not worth a line per sentence.
fn decision(r: &Record) -> Option<String> {
    let p = &r.payload;
    if let Some(error) = plain(&p["error"]).or_else(|| plain(&p["audio_error"])) {
        return Some(format!("decision failed: {error}"));
    }
    if p["outcome"] != "send" {
        return None;
    }
    let trigger = plain(&p["trigger"]).unwrap_or_else(|| "d1".into());
    // Night 8 Nov log sweep, finding 9: the triggers in words. "trigger dropped" (a file the user dropped on the notch
    // with his words, decide::send::Trigger::Dropped) read as if the wake had been lost, though the agent woke.
    let trigger = match trigger.as_str() {
        "dropped" => "a drop on the notch".to_string(),
        "send_it" => "send it".to_string(),
        "open_question" => "open question".to_string(),
        "follow_up" => "follow-up".to_string(),
        _ => trigger,
    };
    let model = plain(&p["choice"]).map(|c| format!("; model said {c}{}", p["probabilities"][&c].as_f64().map(|x| format!(" {x:.2}")).unwrap_or_default())).unwrap_or_default();
    let latency = p["latency_ms"].as_f64().map(|ms| format!("; {ms:.0} ms")).unwrap_or_default();
    Some(format!("woke the agent (trigger {trigger}{model}{latency})"))
}
fn switches(p: &Value) -> Option<String> {
    static LAST: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let s = &p["settings"];
    let on = |b: bool| if b { "on" } else { "off" };
    // `Settings` leaves a false `paused` and `screen_off` out of its JSON (settings.rs, `is_false`): a missing switch is
    // false, never "unknown, so off" (night review G1: "Listening off, Seeing off" printed while both were on).
    let flag = |key: &str| s[key].as_bool().unwrap_or(false);
    // Relay mode names itself (28 Oct 17:26: "Agent on (gpt-6-luna)" while Claude via T3 was the agent, and a relay
    // switch with the model unchanged printed no line at all).
    let model = if flag("agent_relay") {
        format!(" (Claude via T3{})", plain(&s["agent_relay_model"]).map(|m| format!(" · {m}")).unwrap_or_default())
    } else {
        plain(&s["agent_model"]["api"]["id"]).map(|m| format!(" ({m})")).unwrap_or_default()
    };
    // A change written into settings.json by something outside Heimdall says so; a menu click, the settings tool or
    // Heimdall's own change does not (bug note 2026-11-08T02-03-45).
    let from = if p["from"] == "settings.json" { " (from settings.json)" } else { "" };
    let line = format!("Listening {}, Seeing {}, Speaking {}, Agent {}{model}, Engine {}",
        on(!flag("paused")), on(!flag("screen_off")), on(flag("speak_replies")), on(!flag("dry_run")), on(flag("engine_on")));
    // Every other setting that changed since the last record, by name (bug note 2026-11-13T13-55-33).
    let changed = changed_settings(s);
    if let Ok(mut last) = LAST.get_or_init(Default::default).try_lock() {
        if last.as_ref() == Some(&line) && changed.is_empty() {
            return None;
        }
        *last = Some(line.clone());
    }
    let changed = if changed.is_empty() { String::new() } else { format!("; changed: {}", changed.join(", ")) };
    Some(format!("{line}{changed}{from}"))
}
/// The settings that differ from the previous record's: `name=value` for a short scalar, the name alone for a list, an
/// object, a long string or anything named like a credential. Nothing on the first record.
fn changed_settings(s: &Value) -> Vec<String> {
    static LAST: OnceLock<Mutex<Option<serde_json::Map<String, Value>>>> = OnceLock::new();
    let Some(now) = s.as_object() else { return vec![] };
    let Ok(mut last) = LAST.get_or_init(Default::default).try_lock() else { return vec![] };
    let Some(before) = last.replace(now.clone()) else { return vec![] };
    diff_settings(&before, now)
}
fn diff_settings(before: &serde_json::Map<String, Value>, now: &serde_json::Map<String, Value>) -> Vec<String> {
    let secret = |k: &str| ["key", "token", "secret", "password", "credential", "auth", "cookie"].iter().any(|w| k.contains(w));
    let mut names: Vec<&String> = now.keys().chain(before.keys()).collect();
    names.sort();
    names.dedup();
    names.into_iter().filter(|k| now.get(*k) != before.get(*k)).map(|k| match now.get(k) {
        _ if secret(k) => k.clone(),
        Some(v @ (Value::Bool(_) | Value::Number(_))) => format!("{k}={v}"),
        Some(Value::String(v)) if v.len() <= 60 => format!("{k}={v}"),
        // `Settings` leaves a false flag out of its JSON (`is_false`).
        None if before.get(k).is_some_and(Value::is_boolean) => format!("{k}=false"),
        None | Some(Value::Null) => format!("{k} cleared"),
        Some(_) => k.clone(),
    }).collect()
}
fn barge_in(p: &Value) -> Option<String> {
    let outcome = plain(&p["outcome"])?;
    let heard = p["side_transcript"]["text"].as_str().filter(|t| !t.is_empty()).map(|t| format!("; heard {}", quoted(&Value::from(t)))).unwrap_or_default();
    let reason = plain(&p["reason"]).map(|r| format!(" ({r})")).unwrap_or_default();
    Some(format!("{} spoke while Heimdall talked: {outcome}{reason}{heard}", agent::identity::user_start()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record(kind: &str, payload: Value) -> Record {
        Record { kind: kind.into(), source: "heimdall:voice".into(), utterance_ids: vec![], host_ns: 1, wall_ns: 1, payload }
    }
    fn line_of(kind: &str, payload: Value) -> String {
        evidence(&record(kind, payload)).map(|(l, _)| l).unwrap_or_default()
    }
    fn repeats_of(kind: &str, payload: Value) -> Option<Repeats> {
        evidence(&record(kind, payload)).map(|(_, r)| r)
    }
    /// The 19 Oct switches, as the log now tells them (the log had nothing on them).
    #[test]
    fn a_take_agreed_on_two_takes_is_one_kept_line() {
        let edits = json!([{"kind":"substitution","expected":"in","actual":"and","misheard":false}]);
        let l = line_of("drama3", json!({"event":"sentence_coverage","id":3,"sentence":2,"attempt":1,
            "result":{"accepted":true,"edits":edits,"spellings":[{"kind":"agreed","edits":edits}],"transcript":"x"}}));
        assert_eq!(l, "voice: Jarvis take kept (sentence 2, attempt 2 of 3): in→and heard alike on two takes, the recognizer's spelling");
    }
    #[test]
    fn voice_switches_unloads_and_rejections_are_one_plain_line_each() {
        let l = line_of("drama3", json!({"event":"voice_switch","id":6,"sentence":10,"notice":"Jarvis missed a sentence · lighter voice","reason":"incomplete sentence after retries","text":"Splash lacks MTP for this model and vllm metal turns the cache off while drafting."}));
        assert_eq!(l, "voice: fallback cause=word_check from=jarvis to=lighter error=\"incomplete sentence after retries\" · Jarvis missed a sentence · lighter voice: the lighter voice says \"Splash lacks MTP for this model and vllm metal turns the cache off while drafting.\"");
        let l = line_of("drama3", json!({"event":"fatal","reason":"Drama 3 memory guard: swap grew by 0 MB since Jarvis loaded (52.9 GiB available)"}));
        assert_eq!(l, "voice: Jarvis stopped: Drama 3 memory guard: swap grew by 0 MB since Jarvis loaded (52.9 GiB available) [cause=memory]");
        let l = line_of("drama3", json!({"event":"memory_refusal","reason":"Not enough free memory for Jarvis right now (needs 31.1 GB, 15.7 GB free)","available_bytes":15699345408u64}));
        assert_eq!(l, "voice: Jarvis not loaded: Not enough free memory for Jarvis right now (needs 31.1 GB, 15.7 GB free) [cause=memory]");
        let l = line_of("drama3", json!({"event":"memory_unload","phase":"idle","exit_code":73,"reason":"Swap grew by 54 MB with 20.1 GB free. Using the lighter voice."}));
        assert_eq!(l, "voice: Jarvis exited for memory (exit 73, while idle): Swap grew by 54 MB with 20.1 GB free. Using the lighter voice.");
        assert_eq!(line_of("drama3", json!({"event":"memory_unload","phase":"load","exit_code":73})), "", "the refusal before it says why");
        // A switch caused by a worker's fatal line shows that line's reason, not its JSON.
        let l = line_of("drama3", json!({"event":"voice_switch","notice":"Jarvis stopped · lighter voice","text":"Noted.","reason":"Drama 3: {\"event\":\"fatal\",\"reason\":\"Drama 3 memory guard: swap grew by 0 MB since Jarvis loaded (52.9 GiB available)\"}"}));
        assert_eq!(l, "voice: fallback cause=memory from=jarvis to=lighter error=\"Drama 3 memory guard: swap grew by 0 MB since Jarvis loaded (52.9 GiB available)\" · Jarvis stopped · lighter voice: the lighter voice says \"Noted.\"");
        // 26 Oct 01:36: a "not loaded" switch names the failure that left Jarvis down, not only the status note.
        let l = line_of("drama3", json!({"event":"voice_switch","notice":"Jarvis not loaded · lighter voice","text":"I've queued both.","reason":"Drama 3 unavailable. Using the lighter voice.","cause":"fd_exhausted","from":"jarvis","to":"lighter","error":"Too many open files (os error 24)"}));
        assert_eq!(l, "voice: fallback cause=fd_exhausted from=jarvis to=lighter error=\"Too many open files (os error 24)\" · Jarvis not loaded · lighter voice: the lighter voice says \"I've queued both.\" — Drama 3 unavailable. Using the lighter voice.");
        assert_eq!(line_of("drama3", json!({"event":"failure","reason":"Too many open files (os error 24)"})), "voice: Jarvis failed: Too many open files (os error 24) [cause=fd_exhausted]");
        assert_eq!(line_of("drama3", json!({"event":"load_retry","reason":"sidecar load: channel is empty and sending half is closed"})), "voice: Jarvis load failed, trying once more: sidecar load: channel is empty and sending half is closed");
        // Distinct events are never folded into a count; looping states are.
        assert_eq!(repeats_of("drama3", json!({"event":"voice_switch","text":"a"})), Some(Repeats::Kept));
        assert_eq!(repeats_of("drama3", json!({"event":"memory_refusal","reason":"x"})), Some(Repeats::Folded));
        assert_eq!(repeats_of("send_decision", json!({"outcome":"send"})), Some(Repeats::Kept));
        assert_eq!(repeats_of("health", json!({"component":"drama3","state":"unavailable","reason":"Jarvis voice loading"})), Some(Repeats::Folded));
        let l = line_of("drama3", json!({"event":"sentence_coverage","id":12,"sentence":2,"attempt":0,"result":{"accepted":false,"edits":[{"actual":"cash","expected":"cache","kind":"substitution"}],"transcript":"Any engine that can't keep the cash on while drafting is marked as failing."}}));
        assert_eq!(l, "voice: Jarvis take rejected by the word check (sentence 2, attempt 1 of 3): cache→cash; heard \"Any engine that can't keep the cash on while drafting is marked as failing.\"");
        assert_eq!(line_of("drama3", json!({"event":"ready","load_ms":5345.5})), "voice: Jarvis ready, loaded in 5.3 s");
        assert_eq!(line_of("drama3", json!({"event":"ready","engine":"rust","load_ms":5345.5})), "voice: Jarvis ready (Rust engine), loaded in 5.3 s");
        assert_eq!(line_of("drama3", json!({"event":"engine_fallback","from":"rust","to":"python","reason":"sidecar load: channel is empty and sending half is closed"})),
            "voice: Jarvis's Rust engine failed to start, the Python worker takes over until restart: sidecar load: channel is empty and sending half is closed");
        // A take kept after every retry, in the same request or from an earlier attempt, and a reload after an interruption.
        let l = line_of("drama3", json!({"event":"sentence_coverage","sentence":1,"attempt":2,"result":{"accepted":true,"kept_after_retries":true,"edits":[{"actual":"yarovit","expected":"jarovit","kind":"substitution"}],"transcript":"The Baltic Slavs also had a dedicated war god Yarovit."}}));
        assert_eq!(l, "voice: Jarvis take kept (sentence 1, attempt 3 of 3): jarovit→yarovit, his best take rather than the lighter voice; heard \"The Baltic Slavs also had a dedicated war god Yarovit.\"");
        let l = line_of("drama3", json!({"event":"take_kept","sentence":3,"attempt":0,"errors":1,"reason":"word coverage"}));
        assert_eq!(l, "voice: Jarvis take kept (sentence 3, attempt 1 of 3): his best take rather than the lighter voice after word coverage");
        let l = line_of("drama3", json!({"event":"interrupted_restart","reason":"Drama 3 cancellation deadline"}));
        assert_eq!(l, "voice: Jarvis reloading at once: Drama 3 cancellation deadline after an interruption (not counted as a failure)");
        // Accepted takes, word-check stalls (the rejection says it) and memory_wait repeats stay out.
        assert_eq!(line_of("drama3", json!({"event":"sentence_coverage","result":{"accepted":true}})), "");
        assert_eq!(line_of("drama3", json!({"event":"batch","first":1,"rows":3,"lead_s":2.46})), "voice: Jarvis batch: sentences 2 to 4 together, 2.5 s of audio ahead");
        assert_eq!(line_of("drama3", json!({"event":"stall","reason":"word coverage"})), "");
        assert_eq!(line_of("drama3", json!({"event":"memory_wait","reason":"x"})), "");
        // A drop with no fallback line still carries its cause class for the night pass's sweep (9 Nov 12:38).
        assert_eq!(line_of("drama3", json!({"event":"fatal","reason":"Need 55 GB available before loading Drama 3"})),
            "voice: Jarvis stopped: Need 55 GB available before loading Drama 3 [cause=memory]");
        assert_eq!(line_of("drama3", json!({"event":"previous_worker_reaped","waited_ms":1830.0})),
            "voice: Jarvis's reload waited 1.8 s for the stopped worker to free its memory");
        // The memory line: one line when he gives way and one when he returns; each held line is logged quietly.
        let l = line_of("drama3", json!({"event":"memory_give_way","reason":"Memory would pass 105 GB (80.0 GB in use plus 30.0 GB needed for Arsenal). Jarvis unloaded to free memory; Phonon S speaks.","used_bytes":85_899_345_920u64,"need_bytes":32_212_254_720u64,"needed_by":["Arsenal"]}));
        assert_eq!(l, "voice: Jarvis gives way to Phonon S: Memory would pass 105 GB (80.0 GB in use plus 30.0 GB needed for Arsenal). Jarvis unloaded to free memory; Phonon S speaks. [needed by Arsenal]; he returns once memory is back");
        assert_eq!(line_of("drama3", json!({"event":"memory_unload","phase":"give_way","reason":"x"})), "");
        assert_eq!(line_of("drama3", json!({"event":"memory_line","text":"Noted.","reason":"x"})), "");
        assert_eq!(line_of("drama3", json!({"event":"memory_return","used_bytes":51_539_607_552u64})),
            "voice: Jarvis reloading: memory back under the 105 GB line with room for him (48.0 GB in use)", "GiB, as the give-way line");
        assert_eq!(line_of("drama3", json!({"event":"stall","reason":"duration or stall","sentence":0,"attempt":2})), "voice: Jarvis take failed (sentence 0, attempt 3 of 3): duration or stall");
    }
    /// Bug note 2026-11-08T02-03-45: every give-way, hold and return is one voice: line with the reason, the GB figures,
    /// the need file and the hold; an unload by a switch says so, so it is never read as the memory line's.
    #[test]
    fn give_way_hold_return_and_switch_unloads_each_say_why() {
        let gib = 1u64 << 30;
        let reason = "Memory would pass 105 GB (46.0 GB in use plus 62.0 GB needed for Vella Nemotron (angel-nemotron-wer.json)). Jarvis unloaded to free memory; Phonon S speaks.";
        let l = line_of("drama3", json!({"event":"memory_give_way","reason":reason,"used_bytes":46 * gib,"need_bytes":62 * gib,
            "needed_by":["Vella Nemotron (angel-nemotron-wer.json)"],"hold_s":120.0}));
        assert_eq!(l, format!("voice: Jarvis gives way to Phonon S: {reason} [needed by Vella Nemotron (angel-nemotron-wer.json)]; he returns once memory stays back for 2 min"));
        let l = line_of("drama3", json!({"event":"memory_hold","reason":"Not enough room under 105 GB for Jarvis (90.0 GB in use). Phonon S speaks.","used_bytes":90 * gib,"hold_s":60.0}));
        assert_eq!(l, "voice: Jarvis held out for memory: Not enough room under 105 GB for Jarvis (90.0 GB in use). Phonon S speaks.; he returns once memory stays back for 1 min");
        let l = line_of("drama3", json!({"event":"memory_return","used_bytes":48 * gib,"hold_s":60.0,"out_s":372.4,"released":true}));
        assert_eq!(l, "voice: Jarvis reloading: memory back under the 105 GB line with room for him (48.0 GB in use); out 6 min 12 s, the need released and a 1 min hold waited out");
        let l = line_of("drama3", json!({"event":"memory_return","used_bytes":48 * gib,"hold_s":120.0,"out_s":120.0,"released":false}));
        assert_eq!(l, "voice: Jarvis reloading: memory back under the 105 GB line with room for him (48.0 GB in use); out 2 min after a 2 min hold");
        // 01:38:01 and 01:46:20 as they would read now.
        let l = line_of("drama3", json!({"event":"unwanted","reason":"Speaking off","resident":true,"held":null,"used_bytes":46 * gib}));
        assert_eq!(l, "voice: Jarvis unloaded: Speaking off (a switch, not the memory line; 46.0 GB in use)");
        let l = line_of("drama3", json!({"event":"unwanted","reason":"Speaking off","resident":false,"held":"Memory would pass 105 GB.","used_bytes":0}));
        assert_eq!(l, "voice: Jarvis no longer wanted: Speaking off (a switch, not the memory line); still held for memory: Memory would pass 105 GB.");
        assert_eq!(line_of("drama3", json!({"event":"wanted","reason":"Speaking on","held":null,"used_bytes":50 * gib})), "voice: Jarvis wanted: Speaking on; 50.0 GB in use; loading");
        assert_eq!(line_of("drama3", json!({"event":"wanted","reason":"Speaking on","held":"Memory would pass 105 GB."})),
            "voice: Jarvis wanted: Speaking on; held for memory until it is back: Memory would pass 105 GB.");
        // Distinct events: two give-ways a minute apart are two lines, never folded into a count.
        assert_eq!(evidence(&record("drama3", json!({"event":"unwanted","reason":"Speaking off","resident":true}))).map(|(_, r)| r), Some(Repeats::Kept));
        assert_eq!(evidence(&record("drama3", json!({"event":"memory_hold","reason":"x"}))).map(|(_, r)| r), Some(Repeats::Kept));
    }
    #[test]
    fn health_agent_decider_settings_and_capture_lines() {
        assert_eq!(line_of("health", json!({"component":"drama3","state":"unavailable","reason":"Jarvis voice loading","previous":null})), "health: drama3 unavailable: Jarvis voice loading");
        assert_eq!(line_of("health", json!({"component":"speakers","state":"recovered","reason":null,"previous":"Speaker activation failed: database is locked"})), "health: speakers recovered (was: Speaker activation failed: database is locked)");
        assert_eq!(line_of("health", json!({"component":"harness","state":"restarting","cue":"Restarting { key: \"harness\", part: Agent, label: \"Restarting the \\\"agent\\\"…\" }","reason":"AuthBlocked(\"Agent off; operation cancelled\")"})),
            "island: harness restarting: Restarting the \"agent\"… (AuthBlocked(\"Agent off; operation cancelled\"))");
        assert_eq!(line_of("health", json!({"component":"notice","state":"notice","cue":"Notice { part: Speech, label: \"Jarvis missed a sentence · lighter voice\" }","reason":null})), "island: notice: Jarvis missed a sentence · lighter voice");
        let r = Record { kind: "agent_state".into(), source: "agent:test-a".into(), utterance_ids: vec![], host_ns: 1, wall_ns: 1, payload: json!({"state":"Working","problem":null,"queue_depth":2,"unresolved":["a"]}) };
        assert_eq!(evidence(&r).unwrap().0, "agent: Working (2 queued)");
        let mut same = r.clone();
        same.payload["unresolved"] = json!(["b"]);
        assert!(evidence(&same).is_none(), "same state, other ids: no new line");
        let mut failed = r.clone();
        failed.payload = json!({"state":"NeedsReview","problem":"Anthropic 529 overloaded","queue_depth":0});
        assert_eq!(evidence(&failed).unwrap().0, "agent: NeedsReview — Anthropic 529 overloaded");
        assert_eq!(line_of("send_decision", json!({"outcome":"send","trigger":"name","choice":"not_for_heimdall","probabilities":{"not_for_heimdall":0.81},"latency_ms":85.2,"prompt":"x".repeat(8000)})),
            "decider: woke the agent (trigger name; model said not_for_heimdall 0.81; 85 ms)");
        // Night 8 Nov log sweep, finding 9 (8 Nov 01:19:16, a screenshot dropped on the notch at 01:19:08).
        assert_eq!(line_of("send_decision", json!({"outcome":"send","trigger":"dropped","choice":"send","probabilities":{"send":0.97},"latency_ms":101.0})),
            "decider: woke the agent (trigger a drop on the notch; model said send 0.97; 101 ms)");
        assert_eq!(line_of("send_decision", json!({"outcome":"send","trigger":"open_question","latency_ms":90.0})), "decider: woke the agent (trigger open question; 90 ms)");
        assert_eq!(line_of("send_decision", json!({"outcome":"not_for_heimdall","pending_truncated":true,"prompt":"x"})), "", "not every sentence");
        assert_eq!(line_of("send_decision", json!({"error":"decider owner failed: deadline"})), "decider: decision failed: decider owner failed: deadline");
        let s = json!({"settings":{"paused":false,"screen_off":false,"speak_replies":true,"dry_run":false,"engine_on":false,"agent_model":{"api":{"id":"claude-opus-5-5"}},"anthropic_api_key":"sk-ant-api03-secretsecretsecret"}});
        // Other tests share the last-settings state: the first line may name settings changed since theirs.
        assert!(line_of("settings_applied", s.clone()).starts_with("settings: Listening on, Seeing on, Speaking on, Agent on (claude-opus-5-5), Engine off"));
        assert!(!line_of("settings_applied", s.clone()).contains("anthropic_api_key=") , "no credential value");
        let mut relay = s.clone();
        relay["settings"]["agent_relay"] = json!(true);
        relay["settings"]["agent_relay_model"] = json!("claude-sonnet-5-5");
        let l = line_of("settings_applied", relay);
        assert!(l.starts_with("settings: Listening on, Seeing on, Speaking on, Agent on (Claude via T3 · claude-sonnet-5-5), Engine off"), "relay mode names itself: {l}");
        // G1 (20 Oct 18:16:30 "settings: Listening off, Seeing off, Speaking off, Agent on" while both ran): the real
        // serialised Settings omit `paused` and `screen_off` when false.
        let mut real = crate::settings::Settings { dry_run: false, engine_on: true, speak_replies: false, ..Default::default() };
        let serialised = json!({"settings": real.clone()});
        assert!(serialised["settings"].get("paused").is_none() && serialised["settings"].get("screen_off").is_none(), "the fixture is the omitting form");
        assert!(line_of("settings_applied", serialised).starts_with("settings: Listening on, Seeing on, Speaking off, Agent on"));
        real.paused = true;
        real.screen_off = true;
        assert!(line_of("settings_applied", json!({"settings": real})).starts_with("settings: Listening off, Seeing off, Speaking off, Agent on"));
        // Bug note 2026-11-08T02-03-45: a change read from settings.json names its source.
        let mut file = json!({"settings": real, "from": "settings.json"});
        file["settings"]["speak_replies"] = json!(true);
        let l = line_of("settings_applied", file);
        assert!(l.contains("Engine on") && l.ends_with("(from settings.json)"), "{l}");
        assert_eq!(line_of("capture_gap", json!({"reason":"native clock discontinuity without queue loss"})), "capture: capture_gap (heimdall:voice): reason: native clock discontinuity without queue loss");
        assert_eq!(line_of("utterance", json!({"decision":{"audio_error":"utterance waveform unavailable after capture gap","status":"unavailable_audio"},"text":"private words"})),
            "listen: utterance (heimdall:voice): decision.audio_error: utterance waveform unavailable after capture gap");
        assert_eq!(line_of("tts_error", json!({"error":"speech interrupted","text":"On the left it shows the one thing."})), "speech: lighter voice failed: speech interrupted (sentence \"On the left it shows the one thing.\")");
        assert_eq!(line_of("agent_start_failed", json!({"error":"Agent off; operation cancelled"})), "agent: agent_start_failed (heimdall:voice): error: Agent off; operation cancelled");
        assert_eq!(line_of("capture_discontinuity", json!({"capture_flag":true,"clock_gap":true})), "capture: capture_discontinuity (heimdall:voice): capture_flag: true; clock_gap: true");
        assert_eq!(line_of("invariant_violation", json!({"check":"agent_queue_depth","evidence":{"evidence_id":10009056,"queue_depth":2,"unresolved":["a","b","c"]}})), "check: agent_queue_depth: queue_depth=2, unresolved=3 items");
        assert_eq!(line_of("invariant_violation", json!({"check":"drama3_problem:heimdall:voice","evidence":{"event":"memory_refusal"}})), "", "the drama3 line already says it");
        // Bug note 2026-11-13T13-55-33: deliveries and voice replies are events with a line; transcripts stay out.
        assert_eq!(line_of("agent_delivery", json!({"message_id":"m-7","error":null,"transport_accepted":true})), "agent: delivery m-7 accepted by the agent");
        assert_eq!(line_of("voice_reply", json!({"outcome":"nothing"})), "speech: voice reply: nothing");
        assert_eq!(line_of("transcript_segment", json!({"text":"hello"})), "");
    }
    // ---- every event (bug note 2026-11-13T13-55-33) ----
    /// Every record kind in a day of the live store (12–13 Nov, 70 kinds, `lab/event-log-audit-2026-11-13.md`) plus the
    /// event kinds the code emits: each is exactly one line, or is a listed quiet kind with none.
    #[test]
    fn every_kind_of_a_live_day_is_one_line_or_listed_quiet() {
        let events: Vec<(&str, Value, &str)> = vec![
            ("agent_delivery", json!({"message_id":"m-1","transport_accepted":false,"error":"Agent off"}), "agent: delivery m-1 refused: Agent off"),
            ("agent_delivery_resolved", json!({"id":"d-2","outcome":"Completed","reason":null}), "agent: delivery d-2 resolved: Completed"),
            ("agent_wake", json!({"urgent":true,"image_ids":["a","b"]}), "agent: woke (urgent), 2 images"),
            ("agent_steer", json!({"image_ids":[],"delivered":true}), "agent: steered the running turn (delivered)"),
            ("notch_drop", json!({"label":"shot.png","kind":"image","items":1,"owner_speaking":false}), "notch: drop received: 1 image (shot.png)"),
            ("notch_drop_delivery", json!({"label":"shot.png","lines":2,"images":1,"joined":true}), "notch: drop delivered to the agent (shot.png, 1 image, joined to an utterance)"),
            ("speak_back", json!({"text":"The build is green and pushed.","language":"en","enabled":true}), "speech: the agent asked to speak 6 words (en)"),
            ("voice_ack", json!({"outcome":"playing","decider_line":"ok-3"}), "speech: acknowledgement playing (line ok-3)"),
            ("voice_ack", json!({"reason":"agent_off"}), "speech: no acknowledgement (agent_off)"),
            ("voice_ack_playback", json!({"clip_id":"c9","outcome":"played","reason":null,"error":null,"elapsed_ms":412.3}), "speech: acknowledgement c9 played after 412 ms"),
            ("voice_reply_playback", json!({"clip_id":"r1","outcome":"failed","error":"device gone","elapsed_ms":3.0}), "speech: voice-bank reply r1 failed: device gone after 3 ms"),
            ("speech_stopped", json!({"reason":"guest_voice","speaker":"Dave"}), "speech: stopped (guest_voice) by Dave"),
            ("speech_held", json!({"reason":"in a call"}), "speech: speech_held (heimdall:voice): reason: in a call"),
            ("echo_guard", json!({"outcome":"echo_words_left_out","heard":"private words","kept":"words"}), "listen: echo guard: echo_words_left_out"),
            ("hotkey", json!({"key":"wake","woke":true,"lines":3}), "hotkey: wake pressed: woke the agent with 3 line(s)"),
            ("hotkey", json!({"key":"stop","speaking":true}), "hotkey: stop pressed while Heimdall spoke"),
            ("island_report", json!({"event":"shown","kind":"Agent","label":"Working","waited_ms":12}), "island: opened Agent \"Working\" after 12 ms"),
            ("island_report", json!({"event":"left","kind":"Agent","shown_ms":2400,"why":"done"}), "island: closed Agent after 2400 ms (done)"),
            ("island_cue", json!({"cue":"agent_sent"}), "island: sent to the agent"),
            ("island_cue", json!({"cue":"agent_notice","kind":"progress","text":"Checking the build"}), "island: agent notice (progress): \"Checking the build\""),
            ("command_cue", json!({"phase":"heard","id":4}), "command: cue heard #4"),
            ("command_execution", json!({"cue_id":4,"command":"pause"}), "command: voice command #4 ran: pause"),
            ("screen_command", json!({"screen_off":true}), "command: screen command: Seeing is off, nothing captured"),
            ("heimdall_off", json!({"reason":"menu"}), "listen: Listening off (menu)"),
            ("listening_changed", json!({"speaker":"Dave","on":true,"by":"phone"}), "listen: listening to Dave on (by phone)"),
            ("source", json!({"id":"mic:x","label":"Shure MV7i","kind":"Microphone","generation":2}), "capture: source added: Microphone \"Shure MV7i\" (generation 2)"),
            // The live shapes (13 Nov 09:08, a phone turn through the relay).
            ("source", json!({"format":{"channels":2,"sample_rate":48000},"generation":1,"id":"mic:Shure","kind":{"InputDevice":{"uid":"x"}},"label":"Shure MV7i [AppleUSBAudioEngine]"}),
                "capture: source added: InputDevice \"Shure MV7i [AppleUSBAudioEngine]\" (generation 1)"),
            ("agent_transport_sent", json!({"wire":{"id":"control-4808440b-a949-4921-923b-bb33c292fcfb","message":"/heimdall_context [Phone · User] That was a knife.","type":"prompt"}}),
                "agent: sent prompt control-4808440b"),
            ("agent_transport_reply", json!({"wire":{"command":"prompt","data":{"disposition":"handled"},"id":"control-4808440b-a949-4921-923b-bb33c292fcfb","success":true,"type":"response"}}),
                "agent: prompt control-4808440b acknowledged (handled)"),
            ("agent_transport_reply", json!({"wire":{"command":"prompt","id":"control-1","success":false,"error":"busy","type":"response"}}),
                "agent: prompt control-1 refused: busy"),
            ("vella_dictation", json!({"expired":"timeout"}), "vella: dictation ended (timeout)"),
            ("agent_look", json!({"display":1,"width":3456,"height":2234}), "agent: looked at display 1 (3456×2234)"),
            // No line of its own: the catch-all names the kind and its state fields, folded when it loops.
            ("agent_catchup", json!({"state":"added","lines":12,"bytes":4000,"reason":"start"}), "agent: agent_catchup (heimdall:voice): state: added; reason: start"),
            ("speaker_name", json!({"name":"Dave","track":"t1"}), "speakers: speaker_name (heimdall:voice)"),
            ("screenshot", json!({"image_id":"i1"}), "heimdall: screenshot (heimdall:voice)"),
            ("source_removed", json!({"reason":"device gone"}), "capture: source_removed (heimdall:voice): reason: device gone"),
        ];
        for (kind, payload, want) in events {
            let got = evidence(&record(kind, payload.clone()));
            let (line, _) = got.unwrap_or_else(|| panic!("{kind} {payload}: no line"));
            assert_eq!(line, want, "{kind}");
            assert!(!line.contains('\n') && line.len() <= LINE_LIMIT + 40, "{kind}: one bounded line");
        }
        // The rest of the day's 70 kinds: a line from their own arm (checked above or in the tests before) or quiet.
        let day = ["model_output", "speaker_probabilities", "speaker_segments_delta", "listen_event", "send_context", "utterance_binding",
            "agent_transport_reply", "agent_context_routing", "send_decision", "agent_reply", "agent_transport_send", "agent_transport_sent", "drama3",
            "mic_voice", "transcript_revision", "agent_context", "barge_in", "transcript_segment", "suggest_decision", "agent_state", "health",
            "speaker_segments_finalized", "speech_split_call_cpu", "speech_split_progress", "voice_ack", "speech_split_asr_output", "source",
            "island_cue", "speak_back", "agent_wake", "source_removed", "source_readded", "gap", "speech_split_admission", "speech_split_completed",
            "capture_discontinuity", "capture_gap", "speech_split_pending", "voice_ack_playback", "island_report", "hotkey", "speech_held",
            "tts_model_output", "invariant_violation", "agent_look", "echo_guard", "settings_applied", "command_cue", "notch_drop", "speech_stopped",
            "notch_drop_delivery", "capture_diagnostic", "health_cue", "speaker_embeddings", "command_decision", "microphone_cue", "heimdall_off",
            "speaker_name", "listening_ready", "heimdall_on", "agent_catchup", "agent_process_ownership", "listening_changed", "screenshot",
            "timeline_settings", "vella_dictation", "command_action", "command_execution", "screen_command", "speech_skipped"];
        assert_eq!(day.len(), 70);
        for kind in day {
            // An empty payload: kinds with their own arm may need fields (and say nothing without them); quiet kinds never write.
            let quiet = QUIET.contains(&kind);
            if quiet {
                assert!(evidence(&record(kind, json!({"text":"x","outcome":"send"}))).is_none(), "{kind} is quiet");
            } else if !["drama3", "agent_state", "health", "invariant_violation", "send_decision", "barge_in", "settings_applied", "timeline_settings",
                "command_action", "island_cue", "listening_ready", "agent_transport_sent", "agent_transport_reply"].contains(&kind) {
                assert!(evidence(&record(kind, json!({}))).is_some(), "{kind}: a line even with an empty payload");
            }
        }
        // A kind nobody has seen yet still gets a line (its kind and state), never its text.
        let l = line_of("brand_new_event", json!({"state":"started","text":"private words","prompt":"secret prompt","nested":{"error":"x"}}));
        assert_eq!(l, "heimdall: brand_new_event (heimdall:voice): state: started");
        // Problems keep their full line; a problem of a quiet kind is still written.
        assert_eq!(line_of("agent_reply", json!({"error":"stream ended"})), "agent: agent_reply (heimdall:voice): error: stream ended");
        assert_eq!(line_of("agent_reply", json!({"event":{"type":"text_delta"}})), "");
        // Stats polls and streamed message parts: no line.
        assert_eq!(line_of("agent_transport_sent", json!({"wire":{"id":"control-7c5f","type":"get_session_stats"}})), "");
        assert_eq!(line_of("agent_transport_reply", json!({"wire":{"message":{"content":"private words"},"type":"message_end"}})), "");
        assert_eq!(line_of("agent_transport_reply", json!({"wire":{"command":"get_session_stats","id":"c","success":true,"type":"response"}})), "");
        assert_eq!(line_of("speak_back", json!({"text":"a b","language":"en","error":"Speak Replies is off"})),
            "speech: the agent asked to speak 2 words (en); error: Speak Replies is off", "an event line keeps its error");
    }
    /// The new lines carry no secret, no typed secret, no spoken or heard words and no message body.
    #[test]
    fn event_lines_never_carry_secrets_or_bodies() {
        let body = "Tell Dave the password is hunter22";
        for (kind, payload) in [
            ("speak_back", json!({"text":body,"language":"en","enabled":true})),
            ("echo_guard", json!({"outcome":"heimdalls_own_voice","heard":body,"kept":body,"reference":body})),
            ("agent_steer", json!({"delivered":true,"text":body})),
            ("unknown_kind", json!({"text":body,"message":body,"detail":body,"state":"ok"})),
            ("agent_delivery", json!({"message_id":"m","transport_accepted":true,"text":body})),
        ] {
            let l = line_of(kind, payload);
            assert!(!l.is_empty() && !l.contains("hunter22") && !l.contains("Dave"), "{kind}: {l}");
        }
        // Settings: every changed setting is named; a credential's value, a list or a long text never shown.
        let before = json!({"speak_replies":true,"paused":true,"anthropic_api_key":"sk-ant-api03-AAAAAAAAAAAAAAAAAA","voice_speed_percent":100,"api_models":[1]});
        let now = json!({"speak_replies":true,"anthropic_api_key":"sk-ant-api03-BBBBBBBBBBBBBBBBBB","voice_speed_percent":120,"api_models":[1,2],
            "agent_speed":"fast","first_run_said":"x".repeat(80)});
        let changed = diff_settings(before.as_object().unwrap(), now.as_object().unwrap()).join(", ");
        assert_eq!(changed, "agent_speed=fast, anthropic_api_key, api_models, first_run_said, paused=false, voice_speed_percent=120");
        assert!(diff_settings(now.as_object().unwrap(), now.as_object().unwrap()).is_empty(), "nothing changed: nothing named");
        // On the line, after the switches and before the source.
        let s = json!({"settings":{"speak_replies":true}, "from":"settings.json"});
        let l = line_of("settings_applied", s);
        assert!(l.starts_with("settings: Listening on") && l.ends_with("(from settings.json)") && !l.contains("AAAA") && !l.contains("BBBB"), "{l}");
        // Actions: typed text, secrets and keys are withheld; switches show their value.
        assert_eq!(action("menu", "SetListening(false)").unwrap(), "action: menu: SetListening(false)");
        assert_eq!(action("phone", "SetSpeakReplies(true)").unwrap(), "action: phone: SetSpeakReplies(true)");
        let typed = format!("AgentWindow(Send {{ text: \"{body}\", steer: false }})");
        assert_eq!(action("menu", &typed).unwrap(), "action: menu: AgentWindow Send (details withheld)");
        let key = "ConnectProviderKey { provider: \"anthropic\", key: \"sk-ant-api03-CCCCCCCCCCCCCCCCCCCC\" }";
        assert_eq!(action("agent tool", key).unwrap(), "action: agent tool: ConnectProviderKey (details withheld)");
        assert_eq!(action("menu", &format!("{:?}", ui::UiAction::Secret(ui::SecretText::new("hunter22")))).unwrap(), "action: menu: Secret(<secret>)");
        assert_eq!(action("menu", "Island(Shown { id: None, phase: \"x\" })"), None, "island telemetry has its own line");
    }
    // ---- end every event ----
    /// 20 Oct 19:20–19:24 the log showed only the provider's 400 four times; each repair step is now its own line.
    #[test]
    fn a_conversation_repair_is_one_plain_agent_line() {
        let r = Record { kind: "agent_history_repair".into(), source: "agent:pi".into(), utterance_ids: vec![], host_ns: 1, wall_ns: 1,
            payload: json!({"state":"repaired_at_start","line":"Repaired the conversation before the provider saw it: 1 image labelled image/png but image/jpeg, relabelled"}) };
        assert_eq!(evidence(&r).unwrap().0, "agent: Repaired the conversation before the provider saw it: 1 image labelled image/png but image/jpeg, relabelled");
    }
    #[test]
    fn lines_are_single_bounded_and_never_carry_secrets() {
        let l = format("agent", "provider said:\n401 Unauthorized\n  Authorization: Bearer abcdefghijklmnop123 api_key=sk-ant-api03-AAAAAAAAAAAAAAAAAAAA \"token\": \"tok_123456789\" password=hunter22 pcm_sha256 b9b12d72b0fe41d3a27b5c5a169cac90c1227c0da8882c01e8a7e5b69f22d431 file 2026-10-14T20-24-54-925Z_01a13c16-e18d-725b-8727-b95eb76195b4.jsonl");
        assert!(!l.contains('\n'));
        for secret in ["abcdefghijklmnop123", "AAAAAAAAAAAAAAAAAAAA", "tok_123456789", "hunter22", "b9b12d72b0fe41d3a27b5c5a169cac90c1227c0da8882c01e8a7e5b69f22d431"] {
            assert!(!l.contains(secret), "{secret} in {l}");
        }
        assert!(l.contains("2026-10-14T20-24-54-925Z_01a13c16-e18d-725b-8727-b95eb76195b4.jsonl"), "{l}");
        assert!(l.starts_with("agent: provider said: 401 Unauthorized"));
        assert!(format("x", &"long ".repeat(400)).len() <= LINE_LIMIT + 10);
        assert_eq!(redact("short token: abc"), "short token: abc");
        assert_eq!(redact("a sk-ant-api03-abcdefgh1234 b"), "a sk-… b");
    }
    #[test]
    fn repeats_are_written_once_per_window_with_their_count() {
        let t = Instant::now();
        let a = "voice: Jarvis not loaded: Not enough free memory (needs 31.1 GB, 15.7 GB free) test-repeat";
        let b = "voice: Jarvis not loaded: Not enough free memory (needs 31.1 GB, 18.0 GB free) test-repeat";
        assert_eq!(admit(a, false, t).as_deref(), Some(a));
        assert_eq!(admit(b, false, t + Duration::from_secs(30)), None, "same apart from numbers");
        assert_eq!(admit(b, true, t + Duration::from_secs(30)).as_deref(), Some(b), "numbers matter: a different line");
        assert_eq!(admit(a, false, t + Duration::from_secs(60)), None);
        assert_eq!(admit(b, false, t + REPEAT_WINDOW + Duration::from_secs(1)), Some(format!("{b} (and 2 more like it in the last 10 min)")));
    }
}
