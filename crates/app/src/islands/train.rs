//! A TrainingLab run as a job island sees it (30 Oct, background-jobs redesign): `Projects/TrainingLab/runs/<run>/`
//! holds `meta.json` (`total_steps`, `started`) and `metrics.jsonl` (`k: m` steps with `loss`, `tokens_per_s`,
//! `sys_mem_gb`; `k: e` evals; `k: end` with a status; `k: resume`). Round 1 of the island design learned that
//! "Training" from command text alone is wrong (edits to `train_v5.py` before the launch looked like training): only a
//! live run says Training, and a stopped or failed one, until it resumes, says Paused.
//!
//! Only the file's tail is read (`TAIL_BYTES`), and only again when its length changed.

use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The tail of `metrics.jsonl` read per look (a run writes about 200 bytes a step).
pub const TAIL_BYTES: u64 = 512 * 1024;
/// Points kept for the loss and speed series (the `line` part takes 40).
pub const SERIES_POINTS: usize = 48;
/// A stop that resumes within this long is a checkpoint restart, not a pause.
pub const PAUSE_AFTER_S: f64 = 60.0;
/// A run that wrote nothing for this long without an `end` has stalled (crashed, or waiting on a lock): Paused.
pub const STALLED_AFTER_S: f64 = 15.0 * 60.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status { Live, Paused, Done, Failed }

/// What the island shows of a run at a moment.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Train {
    pub run: String,
    #[serde(skip)]
    pub dir: PathBuf,
    pub status: Option<Status>,
    /// When the current status began (the run's start, its last resume, or the stop).
    pub since: f64,
    /// The run's first line.
    pub started: f64,
    pub step: Option<u64>,
    pub total: Option<u64>,
    /// The loss smoothed (EMA 0.97 as round 1 drew it, bias-corrected so its start is the early mean, not a climb from
    /// the first value), thinned to `SERIES_POINTS`.
    pub loss: Vec<f64>,
    /// Loss lines seen in the tail (a trigger counts new ones).
    pub loss_n: usize,
    pub last_loss: Option<f64>,
    pub tok_s: Vec<f64>,
    pub mem_gb: Option<f64>,
    /// (step, [(name, value)]), oldest first.
    pub evals: Vec<(u64, Vec<(String, f64)>)>,
    /// The newest line's time.
    pub last_t: f64,
}

impl Status {
    pub fn stage(self) -> Option<&'static str> {
        match self { Status::Live => Some("Training"), Status::Paused => Some("Paused"), _ => None }
    }
}

/// The run as of `now`, from its folder; `upto` reads only lines written by then (replays and tests).
pub fn read(dir: &Path, now: f64, upto: Option<f64>) -> Option<Train> {
    use std::io::{Read, Seek, SeekFrom};
    let path = dir.join("metrics.jsonl");
    let mut f = std::fs::File::open(&path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A tail read starts mid-line: its first line is dropped.
    let lines = text.lines().skip(usize::from(from > 0));
    let meta: Value = std::fs::read(dir.join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    parse(dir, &meta, lines, now, upto)
}

/// `read` on lines already in hand.
pub fn parse<'a>(dir: &Path, meta: &Value, lines: impl Iterator<Item = &'a str>, now: f64, upto: Option<f64>) -> Option<Train> {
    let values: Vec<Value> = lines.filter_map(|l| serde_json::from_str(l).ok()).collect();
    parse_values(dir, meta, values.iter(), now, upto)
}

/// `parse` on lines already decoded (the binding keeps a run's tail decoded between looks).
pub fn parse_values<'a>(dir: &Path, meta: &Value, lines: impl Iterator<Item = &'a Value>, now: f64, upto: Option<f64>) -> Option<Train> {
    let mut t = Train { run: dir.file_name()?.to_string_lossy().into_owned(), dir: dir.to_path_buf(), total: meta["total_steps"].as_u64(), ..Default::default() };
    let (mut ema, mut seen, mut loss, mut speed): (f64, i32, Vec<f64>, Vec<f64>) = (0.0, 0, Vec::new(), Vec::new());
    // The last control line (`end` status and time, or a resume) and the first line's time.
    let mut end: Option<(String, f64)> = None;
    let mut resumed: Option<f64> = None;
    let mut first: Option<f64> = None;
    for v in lines {
        let at = v["t"].as_f64().unwrap_or(0.0);
        if upto.is_some_and(|u| at > u) { break }
        first.get_or_insert(at);
        t.last_t = t.last_t.max(at);
        match v["k"].as_str() {
            Some("m") => {
                if let Some(s) = v["step"].as_u64() { t.step = Some(s) }
                // Step 1's loss is the untrained model's: left out of the curve, as round 1 did.
                if let Some(l) = v["loss"].as_f64().filter(|l| l.is_finite() && v["step"].as_u64().is_none_or(|s| s >= 2)) {
                    ema = ema * 0.97 + l * 0.03;
                    seen += 1;
                    loss.push(ema / (1.0 - 0.97f64.powi(seen)));
                    t.last_loss = Some(l);
                }
                if let Some(s) = v["tokens_per_s"].as_f64().filter(|s| s.is_finite()) { speed.push(s) }
                if let Some(m) = v["sys_mem_gb"].as_f64() { t.mem_gb = Some(m) }
                if end.is_some() { end = None; resumed = Some(at) }
            }
            Some("e") => {
                let evals: Vec<(String, f64)> = v["evals"].as_object().into_iter().flatten()
                    .filter_map(|(k, x)| Some((k.clone(), x.as_f64()?))).collect();
                if !evals.is_empty() { t.evals.push((v["step"].as_u64().unwrap_or(0), evals)) }
            }
            Some("end") => end = Some((v["status"].as_str().unwrap_or("").to_owned(), at)),
            Some("resume") => { end = None; resumed = Some(at) }
            _ => {}
        }
    }
    let first = first?;
    t.started = meta["started"].as_f64().filter(|s| *s > 0.0 && *s <= first).unwrap_or(first);
    t.loss_n = loss.len();
    t.loss = thin(&loss);
    t.tok_s = thin(&speed);
    let now = upto.map_or(now, |u| u.min(now));
    (t.status, t.since) = match &end {
        Some((s, _)) if s == "done" => (Some(Status::Done), end.as_ref().map_or(now, |e| e.1)),
        // A stop that is not resumed within a minute: paused (a checkpoint restart resumes at once).
        Some((s, at)) if now - at >= PAUSE_AFTER_S => (Some(if s == "failed" || s == "stopped" { Status::Paused } else { Status::Failed }), *at),
        Some(_) => (Some(Status::Live), resumed.unwrap_or(t.started)),
        None if now - t.last_t >= STALLED_AFTER_S => (Some(Status::Paused), t.last_t),
        None => (Some(Status::Live), resumed.unwrap_or(t.started)),
    };
    // Every step taken (a final test eval may run for twenty minutes without a line): the training is over.
    if t.total.is_some_and(|n| t.step.is_some_and(|s| s >= n)) && matches!(t.status, Some(Status::Live | Status::Paused)) {
        (t.status, t.since) = (Some(Status::Done), t.last_t);
    }
    Some(t)
}

/// At most `SERIES_POINTS`, evenly picked, the newest always last.
fn thin(v: &[f64]) -> Vec<f64> {
    if v.len() <= SERIES_POINTS { return v.iter().map(|x| round4(*x)).collect() }
    let k = v.len().div_ceil(SERIES_POINTS);
    let mut out: Vec<f64> = v.iter().rev().step_by(k).map(|x| round4(*x)).collect();
    out.reverse();
    out
}

fn round4(x: f64) -> f64 { (x * 10_000.0).round() / 10_000.0 }

/// TrainingLab's runs folder (the personal edition's; the public one has none).
pub fn runs_root() -> Option<PathBuf> {
    if crate::edition::PUBLIC { return None }
    Some(heimdall_home::home_dir().join("Projects/TrainingLab/runs"))
}

/// The run a thread's commands point at: one they name (`TrainingLab/runs/<name>`, a glob prefix like
/// `decider-v5-2026*` matches the newest run under it), else one that started within `HINT_WINDOW` after a
/// training command (`train_v5.py --run`, which names no run folder: trainlog makes it). Newest first.
pub fn find(root: &Path, named: &[String], hints: &[f64]) -> Option<PathBuf> {
    let mut runs: Vec<PathBuf> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("metrics.jsonl").is_file()).collect();
    runs.sort();
    for n in named.iter().rev() {
        if let Some(r) = runs.iter().rev().find(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with(n.as_str()))) {
            return Some(r.clone());
        }
    }
    if hints.is_empty() { return None }
    runs.into_iter().rev().find(|p| {
        let Some(first) = first_time(p) else { return false };
        hints.iter().any(|h| first >= h - 120.0 && first <= h + HINT_WINDOW)
    })
}

/// How long after a training command its run may start (model load, data prep).
pub const HINT_WINDOW: f64 = 15.0 * 60.0;

fn first_time(dir: &Path) -> Option<f64> {
    use std::io::BufRead;
    let f = std::fs::File::open(dir.join("metrics.jsonl")).ok()?;
    let line = std::io::BufReader::new(f).lines().next()?.ok()?;
    serde_json::from_str::<Value>(&line).ok()?["t"].as_f64()
}

/// Run names a command points at: the folder after `TrainingLab/runs/`, cut at the first glob or shell character
/// (`decider-v5-2026*` → `decider-v5-2026`); at least four characters.
pub fn named_runs(text: &str) -> Vec<String> {
    const AT: &str = "TrainingLab/runs/";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = text[from..].find(AT) {
        let start = from + i + AT.len();
        let name: String = text[start..].chars().take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
        from = start;
        let name = name.trim_end_matches('.').to_owned();
        if name.len() >= 4 && !name.starts_with('.') && !out.contains(&name) { out.push(name) }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[Value]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    #[test]
    fn a_run_is_live_paused_after_a_stop_that_is_not_resumed_and_done_at_its_end() {
        let dir = Path::new("/tmp/runs/decider-v5-20261029-050146");
        let meta = serde_json::json!({"total_steps": 2126});
        let l = lines(&[
            serde_json::json!({"k": "m", "t": 100.0, "step": 1, "loss": 3.1, "tokens_per_s": 2700.0}),
            serde_json::json!({"k": "m", "t": 120.0, "step": 2, "loss": 0.5, "tokens_per_s": 2900.0}),
            serde_json::json!({"k": "m", "t": 140.0, "step": 3, "loss": 0.4, "tokens_per_s": 2800.0}),
            serde_json::json!({"k": "end", "t": 150.0, "step": 3, "status": "failed"}),
            serde_json::json!({"k": "resume", "t": 400.0, "step": 3}),
            serde_json::json!({"k": "e", "t": 500.0, "step": 400, "evals": {"auc": 0.9}}),
            serde_json::json!({"k": "end", "t": 600.0, "step": 2126, "status": "done"}),
        ]);
        let at = |now: f64| parse(dir, &meta, l.iter().map(String::as_str), now, Some(now)).unwrap();
        let t = at(145.0);
        assert_eq!((t.status, t.step, t.total, t.since), (Some(Status::Live), Some(3), Some(2126), 100.0));
        assert_eq!(t.loss.len(), 2, "step 1's loss is the untrained model's");
        assert_eq!(t.loss_n, 2);
        assert_eq!(at(170.0).status, Some(Status::Live), "a stop resumed within a minute is a checkpoint restart");
        let p = at(300.0);
        assert_eq!((p.status, p.since), (Some(Status::Paused), 150.0));
        let r = at(450.0);
        assert_eq!((r.status, r.since), (Some(Status::Live), 400.0));
        assert_eq!(at(520.0).evals, vec![(400, vec![("auc".to_string(), 0.9)])]);
        assert_eq!(at(700.0).status, Some(Status::Done));
        let quiet = &l[..3];
        let stalled = parse(dir, &meta, quiet.iter().map(String::as_str), 140.0 + STALLED_AFTER_S + 1.0, None).unwrap();
        assert_eq!((stalled.status, stalled.since), (Some(Status::Paused), 140.0), "silent for 15 min without an end: stalled");
        let all_steps = parse(dir, &serde_json::json!({"total_steps": 3}), quiet.iter().map(String::as_str), 141.0, None).unwrap();
        assert_eq!(all_steps.status, Some(Status::Done), "every step taken: a final eval without lines is not training");
        assert!(parse(dir, &meta, l.iter().map(String::as_str), 50.0, Some(50.0)).is_none(), "nothing written yet");
    }

    #[test]
    fn named_runs_are_folder_prefixes_and_a_training_command_finds_the_run_it_started() {
        assert_eq!(named_runs("D=$(ls -d /Users/t/Work/Projects/TrainingLab/runs/decider-v5-2026*); tail $D/metrics.jsonl"), ["decider-v5-2026"]);
        assert_eq!(named_runs("cat Projects/TrainingLab/runs/.server-8766.log"), Vec::<String>::new());
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("decider-v5-20261029-050146");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("metrics.jsonl"), "{\"k\":\"m\",\"t\":1000.0,\"step\":1,\"loss\":1.0}\n").unwrap();
        assert_eq!(find(d.path(), &["decider-v5-2026".into()], &[]), Some(run.clone()));
        assert_eq!(find(d.path(), &["other".into()], &[]), None);
        assert_eq!(find(d.path(), &[], &[900.0]), Some(run), "started 100 s after the training command");
        assert_eq!(find(d.path(), &[], &[1000.0 + HINT_WINDOW + 10.0]), None);
    }
}
