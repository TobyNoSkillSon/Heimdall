//! New-voice suggestions ("it would be nice to get a little suggestion"; voice card v3, 3 Nov
//! 19:22). A coded voice talking in an open call gets one silent island card: "New voice: “<a sentence it just said>”",
//! up to two guessed names as one-click chips only when there is a real guess, a name field to type or correct the
//! spelling (3 Nov 18:50: "There should be an input so I can choose how to spell it"), and Not important last. The
//! guess is made before the card shows: a known voice it sounds close to, and the names said around its lines that the
//! background model picks and the transcript backs. Several new voices get the same card, one at a time.
//! Rate: one card at a time, at most every `GAP_MS` and `PER_DAY` a day, never sound. The older returning-voice
//! proposal (`voice_namer::pass`) still runs for regulars. Contract: `docs/speakers.md`.

use crate::voice_namer::{self as namer, Line, State};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use speakers::identity::EnrollmentStore;

/// New-voice cards' ids: `voice-new:<code>`; the returning-voice proposal's are the namer's `voice-name:<code>`.
pub const CARD_PREFIX: &str = "voice-new:";
pub const NOT_IMPORTANT: &str = "Not important";
/// The name field's placeholder; the field comes after the names, Not important after it.
pub const NAME_FIELD: &str = "Type the name";
/// Enough said to be worth a card: this many lines and words in the last `RECENT_MS`, the newest within
/// `namer::ACTIVE_MS`.
pub const MIN_LINES: usize = 3;
pub const MIN_WORDS: usize = 12;
pub const RECENT_MS: i64 = 30 * 60_000;
/// New-voice cards: this far apart, and at most this many in a day.
pub const GAP_MS: u64 = 10 * 60_000;
pub const PER_DAY: usize = 6;
/// A new-voice card left unanswered (expired, put away, or its call went quiet): not suggested again for this long.
pub const SNOOZE_MS: u64 = 3 * 3600_000;
/// How long the new-voice card waits.
pub const CARD_MS: u64 = 5 * 60_000;
/// A known voice this close (cosine of the best pair of samples) is a candidate, never a sure one: names match at
/// 0.456 (`docs/speakers.md`), so 0.40–0.456 is "sounds like".
pub const NEAR_MIN: f32 = 0.40;
pub const MAX_CANDIDATES: usize = 2;
/// A sure candidate: confidence high and this many hint lines checked against the transcript (sure names come first).
pub const SURE_HINTS: usize = 2;

/// One new-voice card: the voice, the sentence to quote, and its guessed names (empty: no real guess).
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub code: String,
    pub quote: String,
    pub candidates: Vec<Candidate>,
    /// The model's error, when it failed (the voice-similarity guess still counts).
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub sure: bool,
}

/// What a voice said since `since_ms`: lines, words, the newest line's time and the sentence to quote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heard {
    pub lines: usize,
    pub words: usize,
    pub last_ms: i64,
    pub quote: String,
}

pub fn heard(db: &Connection, code: &str, since_ms: i64) -> Result<Heard> {
    let mut h = Heard::default();
    let mut q = db.prepare_cached("SELECT coalesce(wall_ms,0), coalesce(text,'') FROM utterances WHERE speaker=?1 AND wall_ms>=?2 AND coalesce(text,'')<>'' ORDER BY wall_ms")?;
    let mut newest_full: Option<String> = None;
    let mut newest: Option<String> = None;
    for r in q.query_map(params![code, since_ms], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (wall, text) = r?;
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let n = text.split_whitespace().count();
        h.lines += 1;
        h.words += n;
        h.last_ms = h.last_ms.max(wall);
        if n >= 4 {
            newest_full = Some(text.clone());
        }
        newest = Some(text);
    }
    h.quote = newest_full.or(newest).unwrap_or_default();
    Ok(h)
}

/// A voice still open for a card: in the store, not "Not important", never suggested or its snooze over.
fn suggestible(store: &EnrollmentStore, state: &State, code: &str, now_ms: u64) -> bool {
    let Some(v) = store.voices.get(code) else { return false };
    let s = state.voices.get(code).cloned().unwrap_or_default();
    !v.irrelevant && (s.suggested_ms == 0 || now_ms >= s.snoozed_until)
}

/// The cards of the last day, and whether one may show now.
pub fn may_show(state: &State, now_ms: u64) -> bool {
    let day: Vec<u64> = state.shown.iter().copied().filter(|t| now_ms.saturating_sub(*t) < 24 * 3600_000).collect();
    day.len() < PER_DAY && day.iter().all(|t| now_ms.saturating_sub(*t) >= GAP_MS)
}

/// The new voice due a card now, if any. `in_call`: the coded voices on a track of an open, active call now (the
/// pipeline's view). Of the voices that said enough lately, the one that spoke last; the others wait their turn (one
/// card at a time, `GAP_MS` apart). Its `candidates` are empty: `guess` fills them.
pub fn next(db: &Connection, store: &EnrollmentStore, state: &State, in_call: &[String], now_ms: u64) -> Result<Option<Suggestion>> {
    if !may_show(state, now_ms) {
        return Ok(None);
    }
    let now = now_ms as i64;
    let mut due: Option<(String, Heard)> = None;
    let mut seen: Vec<&String> = vec![];
    for code in in_call {
        if seen.contains(&code) || !suggestible(store, state, code, now_ms) {
            continue;
        }
        seen.push(code);
        let h = heard(db, code, now - RECENT_MS)?;
        if h.lines >= MIN_LINES && h.words >= MIN_WORDS && h.last_ms >= now - namer::ACTIVE_MS && due.as_ref().is_none_or(|(_, d)| h.last_ms > d.last_ms) {
            due = Some((code.clone(), h));
        }
    }
    Ok(due.map(|(code, h)| Suggestion { code, quote: h.quote, candidates: vec![], error: None }))
}

pub const SYSTEM: &str = "You help User find out who one unnamed voice is in his recorded calls (mostly Discord). \
Heimdall, his assistant, labelled it with a code such as \"Speaker Fri 23 Oct 21h #3\"; in the transcript below its \
lines are marked VOICE. Other lines carry their speaker: User, a known person's name, or another code.

List the names this voice may have, most likely first, at most three. Evidence, strongest first:
- the voice says its own name (\"it's Dave\", \"Dave here\");
- someone addresses a person by name and the VOICE answers next (\"Dave, you there?\" then VOICE: \"yeah\");
- others talk to or about the voice by name in a way only it fits.
Not evidence: a name the VOICE itself says to someone else (that is another person); a name in a story, a game, a \
stream or a song. ASR may misspell names; use the spelling the transcript uses most. Never answer User or Heimdall. \
Only names that appear in the transcript. confidence is high only when several moments point at that name and \
nothing points elsewhere, low when it is a hunch; hints are the line numbers that name the person. No name at all is a \
fine answer: User then types it.";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "candidates": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "confidence": {"type": "string", "enum": ["high", "medium", "low"]},
                    "hints": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["name", "confidence", "hints"],
                "additionalProperties": false
            }},
            "reason": {"type": "string"}
        },
        "required": ["candidates", "reason"],
        "additionalProperties": false
    })
}

pub fn call(model: &str, prompt: &str) -> crate::background_call::Call<'static> {
    crate::background_call::Call { job: namer::JOB, model: model.to_owned(), system: SYSTEM, schema: schema(), effort: "medium", prompt: prompt.into(), history: Vec::new(), session: None }
}

pub fn ask_model(e: &crate::plan_refresher::Endpoint, prompt: &str) -> Result<String> {
    let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
    Ok(crate::background_call::ask_blocking(e, &call(&model, prompt))?.text)
}

/// A name a card may offer: a plausible person's name, not the user or Heimdall, not one the user turned down for this voice.
fn plausible(name: &str, declined: &[String]) -> bool {
    let lower = name.to_lowercase();
    !name.is_empty()
        && name.chars().count() <= 40
        && name.split_whitespace().count() <= 3
        && name.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-' || c == '\'')
        && !["owner", "heimdall", "voice", "speaker", "unknown", "yes", "not important", "forget"].contains(&lower.as_str())
        && !declined.iter().any(|d| namer::same_names(d, name))
}

/// The known voice this one sounds closest to, when within `NEAR_MIN` (and under the naming threshold, or it would
/// carry that name already).
pub fn sounds_like(store: &EnrollmentStore, code: &str) -> Option<(String, f32)> {
    let v = store.voices.get(code)?;
    store
        .names
        .iter()
        .filter(|(n, _)| !speakers::identity::NOT_GUESTS.iter().any(|g| g.eq_ignore_ascii_case(n)))
        .filter_map(|(n, e)| {
            v.embeddings.iter().flat_map(|a| e.embeddings.iter().map(move |b| a.cosine(b))).max_by(f32::total_cmp).map(|s| (n.clone(), s))
        })
        .filter(|(_, s)| *s >= NEAR_MIN)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// The model's candidates, checked against the transcript: a real guess only (confidence high or medium, never low) and
/// said in it; a sure one needs confidence high,
/// `SURE_HINTS` hint lines where the voice says it as its own or answers right after someone says it, and the voice
/// never addressing that name.
pub fn verify(answer: &str, lines: &[Line], declined: &[String]) -> Result<Vec<Candidate>> {
    let v: Value = serde_json::from_str(answer).context("answer is not JSON")?;
    let mut out: Vec<Candidate> = vec![];
    for c in v["candidates"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
        let real = matches!(c["confidence"].as_str(), Some("high" | "medium"));
        if !real || !plausible(&name, declined) || out.iter().any(|o| namer::same_names(&o.name, &name)) || !lines.iter().any(|l| namer::says(&l.text, &name)) {
            continue;
        }
        let mut hints = 0;
        for n in c["hints"].as_array().into_iter().flatten().filter_map(Value::as_u64) {
            let Some(l) = lines.iter().find(|l| l.n == n as usize) else { continue };
            let answered = lines.iter().find(|x| x.n > l.n && x.session == l.session).is_some_and(|x| x.own && x.wall_ms - l.wall_ms <= 20_000);
            if namer::says(&l.text, &name) && if l.own { namer::introduces(&l.text, &name) } else { answered } {
                hints += 1;
            }
        }
        let addressed = lines.iter().any(|l| l.own && namer::says(&l.text, &name) && !namer::introduces(&l.text, &name));
        out.push(Candidate { name, sure: c["confidence"] == "high" && hints >= SURE_HINTS && !addressed });
    }
    Ok(out)
}

/// Who the card's voice may be, before the card shows: at most `MAX_CANDIDATES` real guesses, sure ones first, then
/// the model's order, then the known voice it sounds like. `ask` is the background model (None without one: only the
/// voice-similarity guess); a failing model leaves that guess and its error.
pub fn guess(db: &Connection, store: &EnrollmentStore, state: &State, s: &mut Suggestion, ask: Option<&mut dyn FnMut(&str) -> Result<String>>) -> Result<()> {
    let code = s.code.as_str();
    let declined = state.voices.get(code).map(|v| v.declined.clone()).unwrap_or_default();
    let mut candidates: Vec<Candidate> = vec![];
    if let Some(ask) = ask {
        let lines = namer::context(db, code)?;
        if !lines.is_empty() {
            let known: Vec<String> = store.names.keys().cloned().collect();
            let heard_in = store.voices.get(code).map(|v| v.heard_in.clone()).filter(|h| !h.is_empty()).unwrap_or_else(|| "a call".into());
            let prompt = format!("Heard in: {heard_in}.\n{}", namer::prompt(code, &lines, &known, &declined));
            match ask(&prompt).and_then(|a| verify(&a, &lines, &declined)) {
                Ok(c) => candidates = c,
                Err(e) => s.error = Some(format!("{e:#}")),
            }
        }
    }
    candidates.sort_by_key(|c| !c.sure);
    if let Some((name, _)) = sounds_like(store, code).filter(|(n, _)| plausible(n, &declined)) {
        if !candidates.iter().any(|c| namer::same_names(&c.name, &name)) {
            candidates.push(Candidate { name, sure: false });
        }
    }
    candidates.truncate(MAX_CANDIDATES);
    s.candidates = candidates;
    Ok(())
}

/// `lead`, the quote in quotation marks and `tail`, on the island's one line: as many of the quote's words as fit
/// (`ui::island::object::question_fits_one_line`), cut with an ellipsis; `None` when not even three words fit.
pub fn fitted(lead: &str, quote: &str, tail: &str) -> Option<String> {
    let words: Vec<&str> = quote.split_whitespace().collect();
    let line = |n: usize| {
        let mut q = words[..n].join(" ");
        if n < words.len() {
            // "last week,…" reads as "last week…".
            while q.ends_with([',', ';', ':', '-']) {
                q.pop();
            }
            q.push('…');
        }
        format!("{lead}\u{201c}{q}\u{201d}{tail}")
    };
    (words.len().min(3)..=words.len()).rev().filter(|n| *n > 0).map(line).find(|t| ui::island::object::question_fits_one_line(t))
}

/// The card's line: "New voice: “…”", the quote cut to the island's one line.
pub fn card_text(s: &Suggestion) -> String {
    fitted("New voice: ", &s.quote, "").unwrap_or_else(|| "New voice".into())
}
/// The card's answers in order: the guessed names (one click each), then Not important. The name field sits between
/// them (`OtherField::after`: 1).
pub fn card_options(s: &Suggestion) -> Vec<String> {
    s.candidates.iter().take(MAX_CANDIDATES).map(|c| c.name.clone()).chain([NOT_IMPORTANT.to_string()]).collect()
}

#[cfg(test)]
#[path = "voice_suggest_tests.rs"]
mod tests;
