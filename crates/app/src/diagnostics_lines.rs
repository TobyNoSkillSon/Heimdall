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
/// The archive hook: an evidence record that is a failure, fallback, retry, timeout or state change becomes one line.
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
        "settings_applied" => ("settings", switches(p)?, Kept),
        "barge_in" => ("speech", barge_in(p)?, Kept),
        "speech_held" | "speech_skipped" => ("speech", generic(r), Folded),
        "tts_error" => ("speech", format!("lighter voice failed: {} (sentence {})", plain(&p["error"]).unwrap_or_default(), quoted(&p["text"])), Kept),
        "invariant_violation" => ("check", invariant(p)?, Folded),
        "listening_ready" => ("listen", format!("models ready {} ms after Listening on", p["ms_since_on"].as_f64().map_or("?".into(), |ms| format!("{ms:.0}"))), Kept),
        "capture_gap" | "capture_stalled" | "capture_discontinuity" | "capture_diagnostic" | "gap" | "source_removed" | "source_readded" | "microphone_cue" => ("capture", generic(r), Folded),
        "relaunched" | "recovery" | "archive_stopped" | "diagnostic_drops" => ("heimdall", generic(r), Folded),
        // One plain line per conversation repair step (`agent::history`).
        "agent_history_repair" => ("agent", plain(&p["line"])?, Kept),
        "agent_start_failed" | "agent_runtime_ended" | "agent_queue_fallback" | "agent_loop_guard" | "agent_reload" | "agent_reload_stale" | "agent_idle_compaction_failed" | "agent_preflight" | "agent_stop_wait" => ("agent", generic(r), Kept),
        "command_action" if heimdall_archive::diagnostics::problem(kind, p) => ("command", generic(r), Kept),
        _ if heimdall_archive::diagnostics::problem(kind, p) => (area_of(kind), generic(r), Folded),
        _ => return None,
    };
    Some((format(area, &text), repeats))
}
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
        // One greppable event per switch, its fields first so the line bound never cuts them (docs/nightly-build.md).
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
        "fatal" => (format!("Jarvis stopped: {reason}{free}"), Folded),
        "memory_refusal" => (format!("Jarvis not loaded: {reason}"), Folded),
        // At load the refusal or fatal line just before says why.
        "memory_unload" if phase == "loading" && p["reason"].is_null() => return None,
        // The memory line (bug note 2026-11-03T22-38-44): the give-way line says why; the unload just after is quiet.
        "memory_give_way" => {
            let by = p["needed_by"].as_array().map(|w| w.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            (format!("Jarvis gives way to Phonon S: {reason}{}", if by.is_empty() { String::new() } else { format!(" [needed by {by}]") }), Kept)
        }
        "memory_unload" if phase == "give_way" => return None,
        // The same GB (GiB) as the give-way reason (`memory_line::gb`), never this file's decimal `gb`.
        "memory_return" => (format!("Jarvis reloading: memory back under the 95 GB line with room for him ({} GB in use)",
            speak::memory_line::gb(p["used_bytes"].as_u64().unwrap_or(0))), Kept),
        "memory_line" => return None,
        "memory_unload" => (format!("Jarvis exited for memory (exit 73, while {phase}): {reason}"), Folded),
        "failure" => (format!("Jarvis failed: {reason} [cause={}]", speak::drama3::fallback_cause(&reason)), Folded),
        "unloaded" => ("Jarvis unloaded (no longer wanted)".into(), Folded),
        "verifier_unload_pending" | "verifier_reserve_carried" => (format!("word-check decoder still running; {} kept reserved", gb(&p["retained_bytes"]).unwrap_or_default()), Folded),
        _ => return None,
    })
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
    let line = format!("Listening {}, Seeing {}, Speaking {}, Agent {}{model}, Engine {}",
        on(!flag("paused")), on(!flag("screen_off")), on(flag("speak_replies")), on(!flag("dry_run")), on(flag("engine_on")));
    if let Ok(mut last) = LAST.get_or_init(Default::default).try_lock() {
        if last.as_ref() == Some(&line) {
            return None;
        }
        *last = Some(line.clone());
    }
    Some(line)
}
fn barge_in(p: &Value) -> Option<String> {
    let outcome = plain(&p["outcome"])?;
    let heard = p["side_transcript"]["text"].as_str().filter(|t| !t.is_empty()).map(|t| format!("; heard {}", quoted(&Value::from(t)))).unwrap_or_default();
    let reason = plain(&p["reason"]).map(|r| format!(" ({r})")).unwrap_or_default();
    Some(format!("User spoke while Heimdall talked: {outcome}{reason}{heard}"))
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
        assert_eq!(l, "voice: Jarvis stopped: Drama 3 memory guard: swap grew by 0 MB since Jarvis loaded (52.9 GiB available)");
        let l = line_of("drama3", json!({"event":"memory_refusal","reason":"Not enough free memory for Jarvis right now (needs 31.1 GB, 15.7 GB free)","available_bytes":15699345408u64}));
        assert_eq!(l, "voice: Jarvis not loaded: Not enough free memory for Jarvis right now (needs 31.1 GB, 15.7 GB free)");
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
        // The memory line: one line when he gives way and one when he returns; each held line is logged quietly.
        let l = line_of("drama3", json!({"event":"memory_give_way","reason":"Memory would pass 95 GB (70.0 GB in use plus 30.0 GB needed for Arsenal). Jarvis unloaded to free memory; Phonon S speaks.","used_bytes":75_161_927_680u64,"need_bytes":32_212_254_720u64,"needed_by":["Arsenal"]}));
        assert_eq!(l, "voice: Jarvis gives way to Phonon S: Memory would pass 95 GB (70.0 GB in use plus 30.0 GB needed for Arsenal). Jarvis unloaded to free memory; Phonon S speaks. [needed by Arsenal]");
        assert_eq!(line_of("drama3", json!({"event":"memory_unload","phase":"give_way","reason":"x"})), "");
        assert_eq!(line_of("drama3", json!({"event":"memory_line","text":"Noted.","reason":"x"})), "");
        assert_eq!(line_of("drama3", json!({"event":"memory_return","used_bytes":51_539_607_552u64})),
            "voice: Jarvis reloading: memory back under the 95 GB line with room for him (48.0 GB in use)", "GiB, as the give-way line");
        assert_eq!(line_of("drama3", json!({"event":"stall","reason":"duration or stall","sentence":0,"attempt":2})), "voice: Jarvis take failed (sentence 0, attempt 3 of 3): duration or stall");
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
        assert_eq!(line_of("send_decision", json!({"outcome":"not_for_heimdall","pending_truncated":true,"prompt":"x"})), "", "not every sentence");
        assert_eq!(line_of("send_decision", json!({"error":"decider owner failed: deadline"})), "decider: decision failed: decider owner failed: deadline");
        let s = json!({"settings":{"paused":false,"screen_off":false,"speak_replies":true,"dry_run":false,"engine_on":false,"agent_model":{"api":{"id":"claude-opus-5-5"}},"anthropic_api_key":"sk-ant-api03-secretsecretsecret"}});
        assert_eq!(line_of("settings_applied", s.clone()), "settings: Listening on, Seeing on, Speaking on, Agent on (claude-opus-5-5), Engine off");
        assert_eq!(line_of("settings_applied", s.clone()), "", "unchanged switches: no line");
        let mut relay = s.clone();
        relay["settings"]["agent_relay"] = json!(true);
        relay["settings"]["agent_relay_model"] = json!("claude-sonnet-5-5");
        assert_eq!(line_of("settings_applied", relay), "settings: Listening on, Seeing on, Speaking on, Agent on (Claude via T3 · claude-sonnet-5-5), Engine off", "relay mode names itself");
        // G1 (20 Oct 18:16:30 "settings: Listening off, Seeing off, Speaking off, Agent on" while both ran): the real
        // serialised Settings omit `paused` and `screen_off` when false.
        let mut real = crate::settings::Settings { dry_run: false, engine_on: true, speak_replies: false, ..Default::default() };
        let serialised = json!({"settings": real.clone()});
        assert!(serialised["settings"].get("paused").is_none() && serialised["settings"].get("screen_off").is_none(), "the fixture is the omitting form");
        assert!(line_of("settings_applied", serialised).starts_with("settings: Listening on, Seeing on, Speaking off, Agent on"));
        real.paused = true;
        real.screen_off = true;
        assert!(line_of("settings_applied", json!({"settings": real})).starts_with("settings: Listening off, Seeing off, Speaking off, Agent on"));
        assert_eq!(line_of("capture_gap", json!({"reason":"native clock discontinuity without queue loss"})), "capture: capture_gap (heimdall:voice): reason: native clock discontinuity without queue loss");
        assert_eq!(line_of("utterance", json!({"decision":{"audio_error":"utterance waveform unavailable after capture gap","status":"unavailable_audio"},"text":"private words"})),
            "listen: utterance (heimdall:voice): decision.audio_error: utterance waveform unavailable after capture gap");
        assert_eq!(line_of("tts_error", json!({"error":"speech interrupted","text":"On the left it shows the one thing."})), "speech: lighter voice failed: speech interrupted (sentence \"On the left it shows the one thing.\")");
        assert_eq!(line_of("agent_start_failed", json!({"error":"Agent off; operation cancelled"})), "agent: agent_start_failed (heimdall:voice): error: Agent off; operation cancelled");
        assert_eq!(line_of("capture_discontinuity", json!({"capture_flag":true,"clock_gap":true})), "capture: capture_discontinuity (heimdall:voice): capture_flag: true; clock_gap: true");
        assert_eq!(line_of("invariant_violation", json!({"check":"agent_queue_depth","evidence":{"evidence_id":10009056,"queue_depth":2,"unresolved":["a","b","c"]}})), "check: agent_queue_depth: queue_depth=2, unresolved=3 items");
        assert_eq!(line_of("invariant_violation", json!({"check":"drama3_problem:heimdall:voice","evidence":{"event":"memory_refusal"}})), "", "the drama3 line already says it");
        // Ordinary records stay out of the log.
        for (kind, payload) in [("agent_delivery", json!({"error":null,"transport_accepted":true})), ("transcript_segment", json!({"text":"hello"})), ("voice_reply", json!({"outcome":"nothing"}))] {
            assert_eq!(line_of(kind, payload), "", "{kind}");
        }
    }
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
