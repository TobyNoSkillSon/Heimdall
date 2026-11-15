//! Memory-search reranking with Qwen3-Reranker-0.6B ("we can probably just use the Qwen re-ranker";
//! "It needs to be tiny and fast ... we are running on the Mac so it's running on the battery").
//!
//! The model runs in the native helper's `--rerank` mode (`embedding/native`, Rerank.swift): its own BF16 weights as
//! published, the MLX buffer cache capped at 256 MB. The process starts on the first search that needs it and is
//! stopped after `IDLE_UNLOAD` without one, so at rest it holds nothing. Only hits with words are scored (`plan`);
//! screen and sound hits keep their places (`apply`). Any failure (missing model, memory refusal, crash, timeout)
//! leaves the embedding order and says why; a failed start waits `RETRY_AFTER` before trying again.
use crate::timeline::Chunk;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{atomic::{AtomicUsize, Ordering}, mpsc, Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

pub const MODEL_DIR: &str = "qwen3-reranker-0.6b";
pub const MODEL_ID: &str = "Qwen/Qwen3-Reranker-0.6B";
/// Default number of worded hits reranked per search (the `memory_rerank` setting; 0 turns reranking off).
pub const DEFAULT_CANDIDATES: usize = 100;
pub const MAX_CANDIDATES: usize = 400;
/// The settings' value in force (`set_candidates`).
static CANDIDATES: AtomicUsize = AtomicUsize::new(DEFAULT_CANDIDATES);
pub fn candidates() -> usize { CANDIDATES.load(Ordering::Relaxed) }
pub fn set_candidates(n: usize) { CANDIDATES.store(n.min(MAX_CANDIDATES), Ordering::Relaxed); }
/// Rows a vector search fetches so that about `n` of them have words (a fifth of hits are screen or sound).
pub fn fetch_for(n: usize, floor: usize) -> usize { (n + n / 4).clamp(floor, crate::timeline::VECTOR_LIMIT) }
/// Characters of a hit's text sent to the model (it reads at most 256 tokens of it).
const DOC_CHARS: usize = 1500;
/// No search for this long: the process exits and its memory is returned.
pub const IDLE_UNLOAD: Duration = Duration::from_secs(60);
const RETRY_AFTER: Duration = Duration::from_secs(300);
const STARTUP: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(30);

/// Indices of the rows the model scores: the first `n` whose text has words, in rank order.
pub fn plan(rows: &[Chunk], n: usize) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, c)| !c.text.trim().is_empty()).map(|(i, _)| i).take(n).collect()
}
/// The text one hit is judged by.
pub fn document(c: &Chunk) -> String {
    let t = c.text.trim();
    match t.char_indices().nth(DOC_CHARS) { Some((at, _)) => t[..at].to_owned(), None => t.to_owned() }
}
/// Reorder the scored rows among their own places by score (ties keep embedding order); every other row (no words,
/// or past the first `n` worded ones) stays exactly where the embedding put it. Scores land in `rerank`.
pub fn apply(mut rows: Vec<Chunk>, scored: &[usize], scores: &[f32]) -> Vec<Chunk> {
    assert_eq!(scored.len(), scores.len());
    let mut order: Vec<usize> = (0..scored.len()).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a]).then(a.cmp(b)));
    let picked: Vec<Chunk> = order.iter().map(|&k| {
        let mut c = rows[scored[k]].clone();
        c.rerank = Some(scores[k] as f64);
        c
    }).collect();
    for (slot, c) in scored.iter().zip(picked) { rows[*slot] = c; }
    rows
}

/// One scoring call's result and cost, for the search log.
#[derive(Debug, Clone)]
pub struct Scored { pub scores: Vec<f32>, pub seconds: f64, pub load_seconds: Option<f64>, pub peak_bytes: u64 }

/// Requests go through a writer thread that owns the helper's stdin: a helper that stops reading blocks that thread,
/// never the search, which times out and kills the helper (ending the write).
struct Process { child: Child, requests: mpsc::Sender<Vec<u8>>, replies: mpsc::Receiver<Result<Value, String>>, load_seconds: f64 }
impl Drop for Process { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

pub struct Reranker {
    models: PathBuf,
    process: Option<Process>,
    sequence: u64,
    last_used: Instant,
    /// A start or request failed: no new start before this time, with the reason.
    blocked: Option<(Instant, String)>,
    /// Whether the idle watcher thread is running.
    watching: bool,
    /// Reply allowances: `TIMEOUT`, plus `STARTUP` for a request that started the helper (shorter in tests).
    timeout: Duration,
    startup: Duration,
}
impl Reranker {
    fn new(models: &Path) -> Self {
        Self { models: models.to_owned(), process: None, sequence: 0, last_used: Instant::now(), blocked: None, watching: false, timeout: TIMEOUT, startup: STARTUP }
    }
    pub fn running(&self) -> bool { self.process.is_some() }
    fn helper(&self) -> PathBuf { self.models.join("embeddinggemma2/native/heimdall-embedding-worker") }
    fn weights(&self) -> PathBuf { self.models.join(MODEL_DIR) }
    fn start(&mut self) -> Result<()> {
        if self.process.is_some() { return Ok(()); }
        if let Some((until, why)) = &self.blocked {
            ensure!(Instant::now() >= *until, "reranker unavailable for now: {why}");
        }
        let helper = self.helper();
        let weights = self.weights();
        ensure!(helper.is_file(), "reranker helper missing at {}", helper.display());
        for f in ["model.safetensors", "config.json", "tokenizer.json"] {
            ensure!(weights.join(f).is_file(), "reranker model file {f} missing in {}", weights.display());
        }
        let started = Instant::now();
        let mut child = Command::new(&helper).arg("--rerank").arg(&weights)
            .env_remove("HEIMDALL_EMBED_LAB").env_remove("HEIMDALL_EMBED_QUANT")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().context("start the reranker")?;
        let stdin = child.stdin.take().context("reranker stdin")?;
        let stdout = child.stdout.take().context("reranker stdout")?;
        let (requests, pending) = mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name("rerank-requests".into()).spawn(move || writer(stdin, pending))?;
        let (tx, replies) = mpsc::sync_channel(1);
        std::thread::Builder::new().name("rerank-replies".into()).spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                let r = match reader.by_ref().take(1 << 20).read_line(&mut line) {
                    Err(_) | Ok(0) => Err("reranker exited".to_owned()),
                    Ok(_) if !line.ends_with('\n') => Err("reranker reply truncated".to_owned()),
                    Ok(_) => serde_json::from_str(&line).map_err(|e| format!("reranker reply: {e}")),
                };
                let failed = r.is_err();
                if tx.send(r).is_err() || failed { break; }
            }
        })?;
        self.process = Some(Process { child, requests, replies, load_seconds: started.elapsed().as_secs_f64() });
        Ok(())
    }
    fn fail(&mut self, why: String) -> anyhow::Error {
        self.process = None;
        self.blocked = Some((Instant::now() + RETRY_AFTER, why.clone()));
        anyhow::anyhow!(why)
    }
    /// Score `documents` against `query` (probability that each answers it), in input order.
    pub fn score(&mut self, query: &str, documents: &[String], more: &mut dyn FnMut() -> bool) -> Result<Scored> {
        ensure!(more(), "rerank cancelled");
        ensure!(!query.trim().is_empty() && !documents.is_empty(), "nothing to rerank");
        // A recent failure or memory refusal: skip until it expires, even with the helper still up.
        if let Some((until, why)) = &self.blocked {
            ensure!(Instant::now() >= *until, "reranker unavailable for now: {why}");
        }
        let fresh = self.process.is_none();
        self.start()?;
        self.sequence += 1;
        let id = self.sequence;
        let mut request = serde_json::to_vec(&json!({"id": id, "query": query, "documents": documents}))?;
        request.push(b'\n');
        let p = self.process.as_mut().unwrap();
        if p.requests.send(request).is_err() { return Err(self.fail("reranker disconnected".into())); }
        let started = Instant::now();
        let limit = self.timeout + if fresh { self.startup } else { Duration::ZERO };
        let reply = loop {
            if !more() { self.process = None; anyhow::bail!("rerank cancelled"); }
            if started.elapsed() >= limit { return Err(self.fail("reranker timed out".into())); }
            match p.replies.recv_timeout(Duration::from_millis(10)) {
                Ok(Ok(r)) if r["id"].as_u64() == Some(id) => break r,
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => return Err(self.fail(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(self.fail("reranker exited".into())),
            }
        };
        self.last_used = Instant::now();
        if let Some(error) = reply["error"].as_str() {
            // A memory refusal: the process stays up (weights resident) but searches skip it for a while.
            let why = format!("reranker: {error}");
            self.blocked = Some((Instant::now() + RETRY_AFTER, why.clone()));
            anyhow::bail!(why);
        }
        if reply["model_id"] != MODEL_ID { return Err(self.fail("reranker model identity mismatch".into())); }
        let scores: Vec<f32> = match serde_json::from_value::<Vec<f32>>(reply["scores"].clone()) {
            Ok(s) if s.len() == documents.len() && s.iter().all(|x| x.is_finite()) => s,
            _ => return Err(self.fail("reranker reply has wrong or non-finite scores".into())),
        };
        self.blocked = None;
        let load = if fresh { reply["stats"]["load_seconds"].as_f64().or(Some(p.load_seconds)) } else { None };
        Ok(Scored { scores, seconds: started.elapsed().as_secs_f64(), load_seconds: load,
            peak_bytes: reply["stats"]["peak_bytes"].as_u64().unwrap_or(0) })
    }
    pub fn unload(&mut self) { self.process = None; }
}

fn writer(mut stdin: ChildStdin, pending: mpsc::Receiver<Vec<u8>>) {
    while let Ok(line) = pending.recv() {
        if stdin.write_all(&line).and_then(|_| stdin.flush()).is_err() { return; }
    }
}

/// The process-wide reranker for one models folder; its idle watcher stops the helper after `IDLE_UNLOAD`.
pub struct Service(Arc<Mutex<Reranker>>);
fn services() -> &'static Mutex<Vec<(PathBuf, Arc<Service>)>> {
    static S: OnceLock<Mutex<Vec<(PathBuf, Arc<Service>)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(vec![]))
}
pub fn service(models: &Path) -> Arc<Service> {
    let mut all = services().lock().unwrap();
    if let Some((_, s)) = all.iter().find(|(p, _)| p == models) { return s.clone(); }
    let s = Arc::new(Service(Arc::new(Mutex::new(Reranker::new(models)))));
    all.push((models.to_owned(), s.clone()));
    s
}
/// The app's reranker (`install` at start with its models folder); none in tests and tools that never install one.
static INSTALLED: OnceLock<Arc<Service>> = OnceLock::new();
pub fn install(models: &Path) { let _ = INSTALLED.set(service(models)); }
pub fn installed() -> Option<Arc<Service>> { INSTALLED.get().cloned() }
/// Stop every reranker process now (Arsenal asked for room, docs/personal/arsenal-yield.md).
pub fn unload_all() {
    // Never waits behind a search: a busy reranker is stopped on the next call (the yield loop repeats), and the
    // search itself skips reranking while Arsenal asks for room.
    for (_, s) in services().lock().unwrap().iter() { if let Ok(mut r) = s.0.try_lock() { r.unload(); } }
}
impl Service {
    pub fn score(&self, query: &str, documents: &[String], more: &mut dyn FnMut() -> bool) -> Result<Scored> {
        let mut r = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let result = r.score(query, documents, more);
        if r.running() && !r.watching {
            r.watching = true;
            let weak = Arc::downgrade(&self.0);
            let _ = std::thread::Builder::new().name("rerank-idle".into()).spawn(move || idle_watch(weak, IDLE_UNLOAD));
        }
        result
    }
    pub fn running(&self) -> bool { self.0.lock().unwrap_or_else(|e| e.into_inner()).running() }
}
fn idle_watch(weak: Weak<Mutex<Reranker>>, idle: Duration) {
    loop {
        std::thread::sleep(Duration::from_secs(1).min(idle));
        let Some(shared) = weak.upgrade() else { return };
        let Ok(mut r) = shared.try_lock() else { continue };
        if r.process.is_none() { r.watching = false; return; }
        if r.last_used.elapsed() >= idle {
            r.unload();
            r.watching = false;
            eprintln!("memory search: reranker unloaded after {}s idle", idle.as_secs());
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk(id: &str, text: &str) -> Chunk {
        serde_json::from_value(json!({"id":id,"session":1,"kind":"text","role":"utterance","source":"mic","start_ns":0,"end_ns":1,
            "start_wall_ns":0,"end_wall_ns":1,"text":text,"details":{},"tags":[],"links":[],"content_hash":"h"})).unwrap()
    }
    #[test]
    fn wordless_hits_keep_their_places_and_worded_ones_follow_the_scores() {
        let rows = vec![chunk("a", "alpha"), chunk("screen", ""), chunk("b", "beta"), chunk("sound", "  "), chunk("c", "gamma"), chunk("d", "delta")];
        let scored = plan(&rows, 3);
        assert_eq!(scored, vec![0, 2, 4], "only the first three worded rows");
        let out = apply(rows, &scored, &[0.1, 0.9, 0.5]);
        let ids: Vec<&str> = out.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "screen", "c", "sound", "a", "d"]);
        assert_eq!(out[0].rerank, Some(0.9f32 as f64));
        assert_eq!(out[5].rerank, None, "past the candidate count: embedding order, no score");
    }
    #[test]
    fn equal_scores_keep_embedding_order_and_long_texts_are_cut() {
        let rows = vec![chunk("a", "x"), chunk("b", "y")];
        let out = apply(rows, &[0, 1], &[0.5, 0.5]);
        assert_eq!(out[0].id, "a");
        let long = chunk("l", &"ż".repeat(4000));
        assert_eq!(document(&long).chars().count(), DOC_CHARS);
    }
    #[test]
    fn a_missing_model_fails_without_starting_anything_and_waits_before_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let s = Service(Arc::new(Mutex::new(Reranker::new(dir.path()))));
        let e = s.score("q", &["d".into()], &mut || true).unwrap_err();
        assert!(format!("{e:#}").contains("helper missing"), "{e:#}");
        assert!(!s.running());
    }
    /// A models folder whose "helper" is `script` (a shell script standing in for the native helper).
    fn fake(script: &str) -> (tempfile::TempDir, Service) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("embeddinggemma2/native/heimdall-embedding-worker");
        std::fs::create_dir_all(helper.parent().unwrap()).unwrap();
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join(MODEL_DIR)).unwrap();
        for f in ["model.safetensors", "config.json", "tokenizer.json"] { std::fs::write(dir.path().join(MODEL_DIR).join(f), "x").unwrap(); }
        let mut r = Reranker::new(dir.path());
        r.timeout = Duration::from_millis(500);
        r.startup = Duration::ZERO;
        (dir, Service(Arc::new(Mutex::new(r))))
    }
    /// Review 13 Nov: a helper that stops reading used to block the request write forever, holding the lock.
    #[test]
    fn a_helper_that_never_reads_times_out_and_is_stopped_then_waits_before_retrying() {
        let (_dir, s) = fake("#!/bin/sh\nexec sleep 30\n");
        let docs = vec!["x".repeat(400_000)];
        let started = Instant::now();
        let e = s.score("q", &docs, &mut || true).unwrap_err();
        assert!(format!("{e:#}").contains("timed out"), "{e:#}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(!s.running());
        let e = s.score("q", &["d".into()], &mut || true).unwrap_err();
        assert!(format!("{e:#}").contains("unavailable for now"), "{e:#}");
    }
    /// Review 13 Nov: after a memory refusal the helper stays up, and searches skip it until the block expires.
    #[test]
    fn a_memory_refusal_blocks_reranking_without_sending_again() {
        let (dir, s) = fake("#!/bin/sh\nwhile read line; do echo x >> \"$0.calls\"; echo '{\"id\":1,\"error\":\"rerank paused: low free memory\"}'; done\n");
        let e = s.score("q", &["d".into()], &mut || true).unwrap_err();
        assert!(format!("{e:#}").contains("rerank paused"), "{e:#}");
        let e = s.score("q", &["d".into()], &mut || true).unwrap_err();
        assert!(format!("{e:#}").contains("unavailable for now"), "{e:#}");
        let calls = std::fs::read_to_string(dir.path().join("embeddinggemma2/native/heimdall-embedding-worker.calls")).unwrap();
        assert_eq!(calls.lines().count(), 1, "the second search did not send");
        s.0.lock().unwrap().unload();
    }
    #[test]
    fn candidate_setting_is_bounded() {
        assert_eq!(fetch_for(100, 100), 125);
        assert_eq!(fetch_for(0, 100), 100);
        assert_eq!(fetch_for(400, 100), crate::timeline::VECTOR_LIMIT);
    }
}

/// Opt-in model test (runs Qwen3-Reranker-0.6B, about 2 GB at peak for a second):
/// `HEIMDALL_RERANK_MODELS=<models folder with embeddinggemma2/native and qwen3-reranker-0.6b> cargo test -p heimdall-archive
/// --lib rerank::model_test -- --ignored`.
#[cfg(test)]
mod model_test {
    use super::*;
    #[test]
    #[ignore = "runs the reranker model; set HEIMDALL_RERANK_MODELS"]
    fn the_model_prefers_the_answer_in_english_and_polish_and_unloads() {
        let models = PathBuf::from(std::env::var("HEIMDALL_RERANK_MODELS").expect("HEIMDALL_RERANK_MODELS"));
        let s = Service(Arc::new(Mutex::new(Reranker::new(&models))));
        let docs: Vec<String> = [
            "The dentist appointment moved to Thursday at four.",
            "We talked about the new graphics card prices.",
            "Wizyta u dentysty została przeniesiona na czwartek na szesnastą.",
            "",
        ].iter().map(|d| d.to_string()).collect();
        let en = s.score("When is my dentist appointment?", &docs, &mut || true).unwrap();
        assert!(en.load_seconds.is_some() && en.scores.len() == 4, "{en:?}");
        assert!(en.scores[0] > en.scores[1] && en.scores[2] > en.scores[1], "{en:?}");
        let pl = s.score("Kiedy mam wizytę u dentysty?", &docs, &mut || true).unwrap();
        assert!(pl.load_seconds.is_none(), "the second call reuses the loaded model");
        assert!(pl.scores[2] > pl.scores[1] && pl.scores[0] > pl.scores[1], "{pl:?}");
        assert!(s.running());
        // The idle watcher stops the helper (a short idle here instead of IDLE_UNLOAD).
        idle_watch(Arc::downgrade(&s.0), Duration::from_millis(1));
        assert!(!s.running());
    }
}
