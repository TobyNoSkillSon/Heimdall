//! `goals-lab replay`: a window of a real Pi session replayed in simulated time through the shadow's real code (the
//! stream's rendering, the triggers, the turn building, the ops' checks and their apply into a scratch islands folder),
//! the model reached through a runner: a command run once per call, the call as JSON on stdin
//! (`shadow::runner_input`: session, system, schema, history, user, effort), printing
//! `{"text": answer, "usage": {"input", "cache_read", "cache_write", "output"}, "ms"}`. Writes `<out>/turns.jsonl` (one
//! line per call) and `<out>/final.json`. Never the app's data: everything lives under `<out>`.

use crate::goals::ops;
use crate::goals::shadow::{runner_input, Answer, Shadow};
use crate::goals::stream::{entry_ms, render};
use crate::islands::files::{write_atomic, Root};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Replay {
    pub session: PathBuf,
    /// Epoch ms.
    pub from: i64,
    pub to: i64,
    /// "default" (the seed goals), "empty", or a goals.json to start from.
    pub seed: String,
    pub runner: String,
    pub out: PathBuf,
    pub effort: String,
    pub compact_tokens: Option<u64>,
    /// A settled.json to start from (`--seed-settled`).
    pub seed_settled: Option<PathBuf>,
}

/// A local or offset ISO time (`2026-10-27T09:00`, `2026-10-27 09:00:00`, `…Z`, `…+01:00`) as epoch ms.
pub fn parse_time(t: &str) -> Result<i64> {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
    let t = t.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(t) { return Ok(d.timestamp_millis()) }
    for fmt in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, fmt) {
            return Local.from_local_datetime(&n).earliest().map(|d| d.timestamp_millis()).context("no such local time");
        }
    }
    bail!("a time like 2026-10-27T09:00, got {t:?}")
}

fn iso(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_millis_opt(ms).single().map(|t| t.format("%Y-%m-%dT%H:%M:%S%:z").to_string()).unwrap_or_default()
}

/// One call through the runner: its answer text, usage and time. A runner that fails or prints no answer is a failed
/// call (the lines go again later, as in the app).
pub fn run_runner(cmd: &str, input: &Value) -> Result<(String, Value, i64)> {
    use std::process::{Command, Stdio};
    let started = std::time::Instant::now();
    let mut child = Command::new("/bin/sh").arg("-c").arg(cmd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
        .with_context(|| format!("runner {cmd:?} did not start"))?;
    child.stdin.take().context("runner stdin")?.write_all(&serde_json::to_vec(input)?)?;
    let out = child.wait_with_output()?;
    ensure!(out.status.success(), "runner exited with {}", out.status);
    let v: Value = serde_json::from_slice(&out.stdout).context("runner printed no JSON")?;
    let text = v["text"].as_str().context("runner answer has no text")?.to_owned();
    let ms = v["ms"].as_i64().unwrap_or(started.elapsed().as_millis() as i64);
    Ok((text, v.get("usage").cloned().unwrap_or(json!({})), ms))
}

/// The session's entries as stream lines in `[from, to]`: (time, kind, line) (`goals-lab stream`).
pub fn stream_lines(session: &Path, from: i64, to: i64) -> Result<Vec<(i64, String, String)>> {
    let len = std::fs::metadata(session)?.len();
    let entries = crate::pi_session::entries(session, len)?;
    let mut out = Vec::new();
    for e in crate::pi_session::branch(&entries) {
        let Some(at) = entry_ms(e) else { continue };
        if at < from || at > to { continue }
        if let Some(l) = render(e, at) {
            let kind = serde_json::to_value(l.kind)?.as_str().unwrap_or("").to_owned();
            for t in l.lines { out.push((at, kind.clone(), t)) }
        }
    }
    Ok(out)
}

/// Totals of the runner's usage over a replay.
fn add_usage(total: &mut Value, u: &Value) {
    for k in ["input", "cache_read", "cache_write", "output"] {
        let n = total[k].as_u64().unwrap_or(0) + u[k].as_u64().unwrap_or(0);
        total[k] = json!(n);
    }
}

/// The replay; returns `final.json`'s object.
pub fn replay(r: &Replay) -> Result<Value> {
    ensure!(r.from < r.to, "--from must be before --to");
    std::fs::create_dir_all(&r.out)?;
    // The lab's own scratch, made fresh each run.
    let root = Root(r.out.join("islands-root"));
    let dir = r.out.join("goals");
    for d in [&root.0, &dir] { if d.exists() { std::fs::remove_dir_all(d)? } }
    root.ensure()?;
    match r.seed.as_str() {
        "default" => {}
        "empty" => write_atomic(&root.file(ops::ID), &Value::Object(ops::fresh()))?,
        path => {
            let v: Value = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("seed {path}"))?)?;
            ensure!(v.is_object(), "seed {path}: not a JSON object");
            write_atomic(&root.file(ops::ID), &v)?;
        }
    }
    // Settled as it stood (items and the titles the user removed: the removed-stay-removed check).
    if let Some(p) = &r.seed_settled {
        let v: Value = serde_json::from_slice(&std::fs::read(p).with_context(|| format!("seed {}", p.display()))?)?;
        ensure!(v.is_object(), "seed {}: not a JSON object", p.display());
        write_atomic(&root.file(crate::goals::settled::ID), &v)?;
    }
    let len = std::fs::metadata(&r.session).with_context(|| format!("session {}", r.session.display()))?.len();
    let entries = crate::pi_session::entries(&r.session, len)?;
    let mut times: Vec<i64> = entries.iter().filter_map(entry_ms).filter(|t| *t > r.from && *t <= r.to).collect();
    times.sort_unstable();
    times.dedup();
    let mut shadow = Shadow::open(&dir, &root, r.from, r.seed == "default")?;
    if let Some(c) = r.compact_tokens { shadow.compact_tokens = c }
    // Everything up to `from` is the past: the first segment's header.
    shadow.ingest(&entries, r.from, Some(r.from))?;
    shadow.fresh = false;
    let turns_path = r.out.join("turns.jsonl");
    let mut turns = std::fs::File::create(&turns_path)?;
    let mut now = r.from;
    let mut next = times.into_iter().peekable();
    let (mut calls, mut segments, mut total) = (0usize, 1usize, json!({}));
    loop {
        let entry = next.peek().copied();
        let due = shadow.due();
        let Some(at) = [entry, due].into_iter().flatten().min() else { break };
        if at > r.to { break }
        now = now.max(at);
        while next.peek().is_some_and(|t| *t <= now) { next.next(); }
        shadow.ingest(&entries, now, Some(now))?;
        let Some(req) = shadow.request(now) else { continue };
        let input = runner_input(&req, &r.effort);
        let (answer, usage, ms) = match run_runner(&r.runner, &input) {
            Ok((text, usage, ms)) => (Answer::Text(text.clone()), usage, ms),
            Err(e) => { eprintln!("goals-lab: call at {} failed: {e:#}", iso(now)); (Answer::Retry(format!("{e:#}")), json!({}), 0) }
        };
        let text = match &answer { Answer::Text(t) => t.clone(), _ => String::new() };
        // The answer arrives after the call's own time.
        now += ms.max(0);
        let done = shadow.settle(&req, answer, now)?;
        calls += 1;
        add_usage(&mut total, &usage);
        let Some(done) = done else { continue };
        if done.ended.is_some() { segments += 1 }
        let line = json!({
            "seg": req.seg, "turn": req.turn, "at": iso(req.at), "trigger": req.why.as_str(),
            "new_lines": req.lines.iter().flat_map(|l| l.lines.clone()).collect::<Vec<_>>(),
            "user": req.user(), "answer": text,
            "ops": done.outcomes.iter().map(|o| o.to_value()).collect::<Vec<_>>(),
            "malformed": done.malformed, "goals_after": Value::Object(done.goals.clone()),
            "settled_after": root.read(crate::goals::settled::ID).map(Value::Object).unwrap_or(Value::Null),
            "usage": usage, "ms": ms, "segment_tokens": done.tokens, "compacted": done.ended.is_some(),
        });
        writeln!(turns, "{line}")?;
    }
    let goals = root.read(ops::ID).map(Value::Object).unwrap_or(Value::Null);
    let settled = root.read(crate::goals::settled::ID).map(Value::Object).unwrap_or(Value::Null);
    let summary = json!({
        "session": r.session, "from": iso(r.from), "to": iso(r.to), "seed": r.seed, "effort": r.effort,
        "calls": calls, "segments": segments, "usage": total, "pending_at_end": shadow.pending.len(), "goals": goals, "settled": settled,
    });
    write_atomic(&r.out.join("final.json"), &summary)?;
    Ok(summary)
}
