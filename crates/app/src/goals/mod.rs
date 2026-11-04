//! Background Tasks ▸ Goals: a cheap model shadows the conversation with one task: did what just
//! happened move a goal forward, finish one, or ask or answer something only the user can do? It keeps
//! `islands/goals.json` true: a few big goals a week or a month ahead, finished tasks collapsing under each, the asks
//! small and separate, worded as motivation, never as obligation ("goals are self-motivating", 16:03).
//!
//! The one job with a rolling context (the 30 Oct stateless rule reversed for this job only, because of the cache):
//! `stream` renders the Pi session entry by entry, `trigger` fires on the user's words and finished tasks (never per
//! line), `segment` keeps the turns so each call pays for its new lines and reads the rest from cache, `ops` checks
//! and applies the model's ops (every op quotes the new lines; caps in code), `shadow` drives them, the same in the app
//! and in `goals-lab`. The design: docs/goals.md.

#[cfg(test)]
pub mod lab;
pub mod ledger;
pub mod ops;
pub mod proof;
pub mod segment;
pub mod settled;
pub mod shadow;
pub mod stream;
pub mod trigger;

use shadow::{Answer, Request, Shadow};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The log and ledger label.
pub const JOB: &str = "goals";
/// Thinking is output, and output is this job's cost: the least effort (the lab measures what it costs in quality).
pub const EFFORT: &str = "low";
/// How often the loop looks at the session file (read only when it grew) and at the triggers.
pub const TICK: Duration = Duration::from_secs(5);

/// Background Tasks ▸ Goals' own model when one is set, else the shared Background Tasks ▸ Model, through the
/// fallback (`background_fallback::route`).
pub fn live_endpoint() -> crate::plan_refresher::Endpoint { crate::background_model::task_endpoint(crate::background_model::Task::Goals) }

/// The call for one turn: the system prompt, the segment's turns as history, the new turn (its lines stable, then the
/// Goals now and the clock), the segment's key as the rolling session.
pub fn call<'a>(model: &str, req: &Request) -> crate::background_call::Call<'a> {
    let mut prompt = crate::background_call::Prompt::default();
    prompt.stable(req.stable.clone());
    prompt.push(req.volatile.clone());
    let history = req.history.iter().map(|t| {
        let mut user = crate::background_call::Prompt::default();
        user.stable(t.user.clone());
        user.push(t.volatile.clone());
        crate::background_call::Turn { user, assistant: t.assistant.clone() }
    }).collect();
    crate::background_call::Call {
        job: JOB, model: model.to_owned(), system: shadow::SYSTEM, schema: ops::schema(), effort: EFFORT, prompt,
        history, session: Some(req.key.clone()),
    }
}

fn now_ms() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64) }

/// One line for the log: what the turn kept and dropped.
pub fn summary(done: &shadow::Done) -> String {
    let kept: Vec<String> = done.outcomes.iter().filter(|o| o.kept).map(|o| format!("{} {}", o.op.op, o.reason)).collect();
    let dropped: Vec<String> = done.outcomes.iter().filter(|o| !o.kept).map(|o| format!("{} ({})", o.op.op, o.reason)).collect();
    let mut t = if kept.is_empty() { "nothing moved".to_owned() } else { format!("kept {}", kept.join(", ")) };
    if !dropped.is_empty() { t.push_str(&format!("; dropped {}", dropped.join(", "))) }
    if let Some(m) = &done.malformed { t.push_str(&format!("; answer malformed: {m}")) }
    t
}

/// Where the loop reads the session from: the path, its inode, how far it read, the size and mtime it saw then, and the
/// last entry id on the branch it took. Kept in `<data>/goals/cursor.json`, so a start resumes where it stopped
/// instead of reading the whole session again.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cursor {
    pub path: PathBuf,
    pub ino: u64,
    pub offset: u64,
    pub len: u64,
    /// Epoch ms.
    pub modified: i64,
    pub last: Option<String>,
}

impl Cursor {
    pub fn load(dir: &Path) -> Cursor {
        std::fs::read(dir.join("cursor.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self, dir: &Path) {
        let r = serde_json::to_value(self).map_err(anyhow::Error::from).and_then(|v| crate::islands::files::write_atomic(&dir.join("cursor.json"), &v));
        if let Err(e) = r { eprintln!("Goals: cursor not saved: {e:#}") }
    }
}

/// Reads what the session file gained since the last look into the stream. The whole file again (the stream's ids keep
/// that from repeating anything) when another file became active, the file was replaced (another inode), shrank, or
/// its mtime went back (rotated and grown past the old offset within a tick), and when the new entries' branch does
/// not continue from the last one taken (Pi branched back: review 31 Oct).
pub fn read_new(shadow: &mut Shadow, cur: &mut Cursor, data: &Path, now: i64) -> anyhow::Result<usize> {
    use std::os::unix::fs::MetadataExt;
    let path = crate::pi_session::session_path(data);
    let meta = match std::fs::metadata(&path) { Ok(m) => m, Err(_) => return Ok(0) };
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
    if path == cur.path && meta.ino() == cur.ino && meta.len() == cur.len && modified == cur.modified { return Ok(0) }
    let replaced = path != cur.path || meta.ino() != cur.ino || meta.len() < cur.offset || modified < cur.modified;
    if replaced { *cur = Cursor { path: path.clone(), ino: meta.ino(), ..Cursor::default() } }
    let (mut entries, mut offset) = crate::pi_session::entries_from(&path, cur.offset, meta.len())?;
    let chain = crate::pi_session::branch(&entries);
    let continues = match (chain.first(), &cur.last) {
        (Some(first), Some(last)) if first["id"].is_string() => first["parentId"].as_str() == Some(last.as_str()),
        _ => true,
    };
    if !continues && cur.offset > 0 {
        (entries, offset) = crate::pi_session::entries_from(&path, 0, meta.len())?;
    }
    let last = crate::pi_session::branch(&entries).iter().rev().find_map(|e| e["id"].as_str().map(str::to_owned));
    // The cursor moves only once the lines are in the stream.
    let n = shadow.ingest(&entries, now, None)?;
    // The first read decides what is the past, even when it held nothing to render (review 31 Oct).
    shadow.fresh = false;
    *cur = Cursor { path, ino: meta.ino(), offset, len: meta.len(), modified, last: last.or(cur.last.take()) };
    cur.save(&shadow.dir);
    Ok(n)
}

/// The T3 session to end now (`background_call::t3::end_session`): the one held, once the job is turned off or its
/// model is no longer on T3 (`on_t3`: what the call about to go out runs on; `None` when no call is resolved).
pub fn session_to_end(held: &mut Option<String>, enabled: bool, on_t3: Option<bool>) -> Option<String> {
    if !enabled || on_t3 == Some(false) { held.take() } else { None }
}

/// The installed loop: the islands host's folder, `<data>/goals` for the stream and the segment. Off (Background
/// Tasks ▸ Goals): nothing is sent, and what was said meanwhile is not news when it comes back on. Agent off or
/// emergency mode's pause: nothing is sent, the lines wait (a turn carries the newest `shadow::TURN_LINES`). Tests
/// never start it.
pub fn spawn_live(data: &Path, gate: Arc<agent::delivery::DeliveryGate>, stop: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(test) { return None }
    let Some(host) = crate::islands::host() else {
        eprintln!("Goals: Floating Islands are not running in this Heimdall; Goals are not kept.");
        return None;
    };
    let (data, root) = (data.to_path_buf(), host.root.clone());
    Some(tokio::spawn(run(data, root, gate, stop)))
}

async fn run(data: PathBuf, root: crate::islands::files::Root, gate: Arc<agent::delivery::DeliveryGate>, mut stop: tokio::sync::watch::Receiver<bool>) {
    let dir = data.join("goals");
    let opened = {
        let (dir, root) = (dir.clone(), root.clone());
        tokio::task::spawn_blocking(move || Shadow::open(&dir, &root, now_ms(), !cfg!(feature = "public"))).await
    };
    let shadow = match opened {
        Ok(Ok(s)) => Arc::new(Mutex::new(s)),
        Ok(Err(e)) => { eprintln!("Goals: not started: {e:#}"); return }
        Err(e) => { eprintln!("Goals: not started: {e}"); return }
    };
    let cursor = Arc::new(Mutex::new(Cursor::load(&dir)));
    // The rolling session on T3, while one is open: ended when the job is turned off or its model leaves T3.
    let mut t3_key: Option<String> = None;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut no_key_logged = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = crate::background_gate::resumed() => {}
            r = stop.changed() => { if r.is_err() || *stop.borrow() { break } else { continue } }
        }
        if *stop.borrow() { break }
        // Turned off: what is said meanwhile is not news. Agent off or emergency mode's pause: the lines wait for the
        // gate (`background_gate::resumed`), as every job's work does.
        let enabled = crate::background_model::enabled(crate::background_model::Task::Goals);
        let open = gate.check().is_ok() && crate::background_gate::emergency_paused().is_none();
        if let Some(k) = session_to_end(&mut t3_key, enabled, None) { crate::background_call::t3::end_session(&k) }
        let step = {
            let (shadow, cursor, data) = (shadow.clone(), cursor.clone(), data.clone());
            tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Request>> {
                let mut s = shadow.lock().unwrap_or_else(|e| e.into_inner());
                let now = now_ms();
                read_new(&mut s, &mut cursor.lock().unwrap_or_else(|e| e.into_inner()), &data, now)?;
                if !enabled { s.forget_pending(); return Ok(None) }
                if !open { return Ok(None) }
                Ok(s.request(now))
            }).await
        };
        let req = match step {
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => continue,
            Ok(Err(e)) => { eprintln!("Goals: session not read: {e:#}"); continue }
            Err(e) => { eprintln!("Goals: {e}"); continue }
        };
        let endpoint = match tokio::task::spawn_blocking(live_endpoint).await { Ok(e) => e, Err(_) => continue };
        // Off T3 now (another pick, a fallback): its thread for this segment is done.
        if let Some(k) = session_to_end(&mut t3_key, true, Some(ui::t3_instance(endpoint.provider).is_some())) { crate::background_call::t3::end_session(&k) }
        let answer = if endpoint.key.is_none() {
            if !no_key_logged { eprintln!("Goals: no model is connected for Background Tasks ▸ Goals; Goals are not kept."); no_key_logged = true }
            None
        } else {
            no_key_logged = false;
            let model = endpoint.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
            if ui::t3_instance(endpoint.provider).is_some() { t3_key = Some(req.key.clone()) }
            Some(match crate::background_call::ask(&endpoint, &call(&model, &req)).await {
                Ok(a) => Answer::Text(a.text),
                Err(e) if crate::background_gate::quiet(&e) => Answer::Paused,
                Err(e) if crate::background_call::settled(&e) => Answer::Settled(format!("{e:#}")),
                Err(e) => Answer::Retry(format!("{e:#}")),
            })
        };
        let lines = req.lines.len();
        let settled = {
            let shadow = shadow.clone();
            tokio::task::spawn_blocking(move || {
                let mut s = shadow.lock().unwrap_or_else(|e| e.into_inner());
                match answer {
                    Some(a) => s.settle(&req, a, now_ms()),
                    // No model: the lines are not news when one is connected.
                    None => { s.forget_pending(); Ok(None) }
                }
            }).await
        };
        match settled {
            Ok(Ok(Some(done))) => {
                eprintln!("Goals: {} ({} lines, segment ~{} tokens)", summary(&done), lines, done.tokens);
                if let Some(old) = &done.ended {
                    eprintln!("Goals: context compacted; a new segment begins");
                    crate::background_call::t3::end_session(old);
                    if t3_key.as_ref() == Some(old) { t3_key = None }
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => eprintln!("Goals: answer not applied: {e:#}"),
            Err(e) => eprintln!("Goals: {e}"),
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
