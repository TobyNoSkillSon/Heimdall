//! The lab opt-in exercises the actual Rust/native boundary, without opening an archive.
//! The other tests drive `MlxEmbedder` against a scripted fake worker that speaks the
//! JSONL protocol, so they need no model, GPU or installed app.
use anyhow::Result;
use heimdall_archive::{
    embedding::{Input, MlxEmbedder, DIMENSIONS, MODEL_ID, MODEL_VERSION},
    store, timeline::{self, BackfillGate, Chunk, Embedder, Model}, Journal,
};
use rusqlite::params;
use serde_json::Value;
use std::{path::{Path, PathBuf}, process::Command, time::{Duration, Instant}};

/// The helper's MLX buffer cache: capped at 256 MB and cleared after every request (the `legacy` A/B switch keeps the
/// old 2 GiB, never cleared). `--cache-policy` loads no weights and runs no inference: MLX reports the limit it set.
#[test]
fn native_cache_policy_without_model() {
    let worker = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/embeddinggemma2/native/heimdall-embedding-worker");
    if !worker.is_file() {
        // models/ is git-ignored: a checkout without the built helper has nothing to check
        eprintln!("skipped: {} missing (scripts/build_embedding_native.py builds it)", worker.display());
        return;
    }
    let policy = |value: Option<&str>| -> serde_json::Value {
        let mut command = Command::new(&worker);
        command.arg("--cache-policy").env_remove("HEIMDALL_EMBED_CACHE_MB").env_remove("HEIMDALL_EMBED_LAB").env_remove("HEIMDALL_EMBED_QUANT");
        if let Some(value) = value { command.env("HEIMDALL_EMBED_CACHE_MB", value); }
        let out = command.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    assert_eq!(policy(None), serde_json::json!({"cache_limit_bytes": 256u64 << 20, "clears": true}));
    assert_eq!(policy(Some("64")), serde_json::json!({"cache_limit_bytes": 64u64 << 20, "clears": true}));
    assert_eq!(policy(Some("legacy"))["clears"], false);
}

#[test]
#[ignore = "requires explicit isolated native model root; run under shared GPU lock"]
fn native_default_query_and_cancellation_restart() {
    let root = PathBuf::from(std::env::var_os("HEIMDALL_EMBED_NATIVE_LAB_MODELS").expect("isolated lab models"));
    assert!(root.is_absolute());
    let mut provider = MlxEmbedder::new(&root);
    let query = provider.query("changing the microphone settings", &mut || true).unwrap();
    assert_eq!(query.model_version, MODEL_VERSION);
    assert_eq!(query.values.len(), DIMENSIONS);
    assert!(query.values.iter().all(|x| x.is_finite()));
    let norm: f32 = query.values.iter().map(|x| x*x).sum();
    assert!((norm - 1.).abs() < 1e-5);
    provider.unload();
    // Cancellation during a fresh process's load drops only that query: the helper keeps
    // starting and answers the next one. No installed app or real archive is involved.
    let started = Instant::now();
    let error = provider.inputs(&[Input::text("cancel this query", true)],
        &mut || started.elapsed() < Duration::from_millis(50)).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    let pid = provider.pid();
    assert!(pid.is_some());
    let restarted = provider.query("changing the microphone settings", &mut || true).unwrap();
    assert_eq!(query.values, restarted.values);
    assert_eq!(provider.pid(), pid);
    provider.unload();
}

/// Text decides behaviour: "n:K" → one-hot vector at K, "fail" → worker-reported error,
/// "crash" → killed by SIGKILL with the request pending (a signal death, like a Metal abort,
/// but SIGKILL writes no crash report and opens no "quit unexpectedly" dialog), "exit" → orderly exit(1) with it pending
/// (like the start-up memory refusal), "nobatch" → garbage reply to a batch.
/// `startup.txt` beside it holds seconds to wait before reading the first request (the real
/// helper's self-test and weight load); "slow:K" answers after 0.5 s.
const FAKE_WORKER: &str = r#"import json, os, signal, sys, time
here = os.path.dirname(os.path.abspath(__file__))
version = open(os.path.join(here, "version.txt")).read()
log = open(os.path.join(here, "log.jsonl"), "a")
def note(o):
    log.write(json.dumps(o) + "\n"); log.flush()
note({"start": os.getpid(), "safe": os.environ.get("HEIMDALL_EMBED_SAFE"), "state": os.environ.get("HEIMDALL_EMBED_STATE"),
      "lab": os.environ.get("HEIMDALL_EMBED_LAB"), "quant": os.environ.get("HEIMDALL_EMBED_QUANT")})
if os.path.exists(os.path.join(here, "startup.txt")):
    time.sleep(float(open(os.path.join(here, "startup.txt")).read()))
def text(inputs):
    return "".join(p.get("text", "") for i in inputs for p in i["parts"])
def vec(t):
    v = [0.0] * 768
    v[int(t.split(":")[1]) % 768] = 1.0
    return v
for line in sys.stdin:
    r = json.loads(line)
    op = r.get("op", "embed")
    note({"op": op, "pid": os.getpid(), "n": len(r.get("items", []))})
    def out(o):
        o["id"] = r["id"]; sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
    if op in ("trim", "usage"):
        out({"usage": {"pid": os.getpid()}}); continue
    if op == "batch":
        texts = [text(i) for i in r["items"]]
        if any("nobatch" in t for t in texts):
            sys.stdout.write("garbage\n"); sys.stdout.flush(); continue
        if any("crash" in t for t in texts):
            os.kill(os.getpid(), signal.SIGKILL)
        out({"vectors": [None if "fail" in t else vec(t) for t in texts],
             "errors": ["bad item" if "fail" in t else None for t in texts],
             "model_id": "google/embeddinggemma-2", "model_version": version, "usage": {},
             "stats": {"seconds": 0.01, "tokens": 10 * len(texts), "windows": len(texts)}})
        continue
    t = text(r["inputs"])
    if "crash" in t:
        os.kill(os.getpid(), signal.SIGKILL)
    if "exit" in t:
        os._exit(1)
    if "fail" in t:
        out({"error": "bad input"}); continue
    if "slow" in t:
        time.sleep(0.5)
    out({"vector": vec(t), "model_id": "google/embeddinggemma-2", "model_version": version})
"#;

struct Fake { _dir: tempfile::TempDir, models: PathBuf, native: PathBuf }
/// The fake worker's interpreter: HEIMDALL_TEST_PYTHON, else the first `python3` on PATH.
/// Never /usr/bin/python3 (Xcode's 3.9 stub). Missing → the test fails with a clear message.
fn python() -> PathBuf {
    let path = std::env::var_os("HEIMDALL_TEST_PYTHON").map(PathBuf::from).unwrap_or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|d| d != Path::new("/usr/bin"))
            .map(|d| d.join("python3"))
            .find(|p| p.is_file())
            .unwrap_or_default()
    });
    assert!(path.is_file(), "fake embedding worker needs a Python: set HEIMDALL_TEST_PYTHON or put python3 on PATH (found {:?})", path);
    // A wrapper script cannot be a shebang interpreter: use the binary it runs.
    let out = Command::new(&path).args(["-I", "-c", "import sys; print(sys.executable)"]).output().expect("run the test Python");
    let real = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert!(real.is_file(), "{} names no interpreter: {:?}", path.display(), out);
    real
}
fn fake(extra: &str) -> Fake {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(MODEL_ID, "google/embeddinggemma-2");
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    let native = models.join("embeddinggemma2/native");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::create_dir_all(models.join("embeddinggemma2/weights")).unwrap();
    std::fs::write(models.join("embeddinggemma2/weights/model.safetensors"), b"").unwrap();
    std::fs::write(native.join("mlx.metallib"), b"").unwrap();
    std::fs::write(native.join("version.txt"), MODEL_VERSION).unwrap();
    let worker = native.join("heimdall-embedding-worker");
    std::fs::write(&worker, format!("#!{} -I\n{FAKE_WORKER}{extra}", python().display())).unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fake { _dir: dir, models, native }
}
impl Fake {
    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.native.join("log.jsonl")).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }
    fn ops(&self) -> Vec<String> {
        self.log().iter().filter_map(|l| l["op"].as_str().map(str::to_owned)).collect()
    }
    fn starts(&self) -> Vec<Value> {
        self.log().into_iter().filter(|l| l.get("start").is_some()).collect()
    }
}
fn input(text: &str) -> Result<Vec<Input>> { Ok(vec![Input::text(text, false)]) }
fn hot(v: &[f32]) -> usize { v.iter().position(|&x| x == 1.).unwrap() }

#[test]
fn batch_replies_map_to_their_items_and_item_errors_stay_local() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    let out = p.embed_inputs_many(vec![input("n:3"), input("fail:4"), Err(anyhow::anyhow!("prepare broke")), input("n:5")], &mut || true);
    assert_eq!(out.len(), 4);
    assert_eq!(hot(out[0].as_ref().unwrap()), 3);
    assert!(out[1].as_ref().unwrap_err().to_string().contains("bad item"));
    assert!(out[2].as_ref().unwrap_err().to_string().contains("prepare broke"));
    assert_eq!(hot(out[3].as_ref().unwrap()), 5);
    // One request carried the three prepared items; the prepare failure never reached it.
    assert_eq!(f.ops(), ["batch"]);
    assert_eq!(f.log().iter().find(|l| l["op"] == "batch").unwrap()["n"], 3);
    // An item error is not a process failure.
    assert!(p.running());
}

#[test]
fn failed_batch_falls_back_to_one_legacy_request_per_item() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    let out = p.embed_inputs_many(vec![input("n:1"), input("nobatch:2"), input("fail:3")], &mut || true);
    assert_eq!(hot(out[0].as_ref().unwrap()), 1);
    assert_eq!(hot(out[1].as_ref().unwrap()), 2);
    assert!(out[2].as_ref().unwrap_err().to_string().contains("bad input"));
    assert_eq!(f.ops(), ["batch", "embed", "embed", "embed"]);
    // The garbage reply is a protocol fault: that process ended, but it is not a crash.
    assert_eq!(f.starts().len(), 2);
    assert!(!p.safe_mode());
}

#[test]
fn a_single_item_uses_the_legacy_request() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    let out = p.embed_inputs_many(vec![input("n:7"), Err(anyhow::anyhow!("gone"))], &mut || true);
    assert_eq!(hot(out[0].as_ref().unwrap()), 7);
    assert_eq!(f.ops(), ["embed"]);
}

/// Mixed precision was measured and rejected (3 Nov): a serving helper never sees the lab switches, even when the
/// app's own environment carries them, so it cannot quantise or switch parts.
#[test]
fn serving_helper_never_inherits_lab_or_quant_switches() {
    std::env::set_var("HEIMDALL_EMBED_LAB", "1");
    std::env::set_var("HEIMDALL_EMBED_QUANT", "mlp:4");
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    assert_eq!(hot(&p.query("n:2", &mut || true).unwrap().values), 2);
    let start = &f.starts()[0];
    assert_eq!(start["lab"], Value::Null);
    assert_eq!(start["quant"], Value::Null);
}

#[test]
fn worker_reported_error_keeps_the_same_process() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    assert!(p.query("fail:1", &mut || true).unwrap_err().to_string().contains("bad input"));
    let pid = p.pid().unwrap();
    assert_eq!(hot(&p.query("n:2", &mut || true).unwrap().values), 2);
    assert_eq!(p.pid(), Some(pid));
    assert_eq!(f.starts().len(), 1);
}

#[test]
fn two_crashes_in_a_row_start_safe_mode_for_that_build_only() {
    let f = fake("");
    let state = f.models.join("state");
    let mut p = MlxEmbedder::with_state(&f.models, Some(state.clone()));
    assert!(p.query("crash:1", &mut || true).unwrap_err().to_string().contains("disconnected"));
    assert!(!p.safe_mode());
    assert!(p.query("crash:2", &mut || true).is_err());
    assert!(p.safe_mode());
    assert_eq!(hot(&p.query("n:3", &mut || true).unwrap().values), 3);
    let starts = f.starts();
    assert_eq!(starts.len(), 3);
    assert_eq!(starts[0]["safe"], Value::Null);
    assert_eq!(starts[1]["safe"], Value::Null);
    assert_eq!(starts[2]["safe"], "1");
    assert!(starts.iter().all(|s| s["state"] == state.to_str().unwrap()));
    // The success reset the count; safe mode stays for this binary, across restarts.
    let record: Value = serde_json::from_slice(&std::fs::read(state.join("rust-worker-crashes.json")).unwrap()).unwrap();
    assert_eq!(record["crashes"], 0);
    assert_eq!(record["safe"], true);
    p.unload();
    let mut again = MlxEmbedder::with_state(&f.models, Some(state.clone()));
    again.query("n:4", &mut || true).unwrap();
    assert_eq!(f.starts()[3]["safe"], "1");
    again.unload();
    // A new build (different size) starts with a clean record.
    std::fs::write(f.native.join("heimdall-embedding-worker"), format!("#!{} -I\n{FAKE_WORKER}# rebuilt\n", python().display())).unwrap();
    let mut rebuilt = MlxEmbedder::with_state(&f.models, Some(state));
    rebuilt.query("n:5", &mut || true).unwrap();
    assert_eq!(f.starts()[4]["safe"], Value::Null);
    assert!(!rebuilt.safe_mode());
}

#[test]
fn a_success_between_crashes_resets_the_count() {
    let f = fake("");
    let mut p = MlxEmbedder::with_state(&f.models, Some(f.models.join("state")));
    assert!(p.query("crash:1", &mut || true).is_err());
    p.query("n:2", &mut || true).unwrap();
    assert!(p.query("crash:3", &mut || true).is_err());
    // A worker-reported error is not a crash either.
    assert!(p.query("fail:4", &mut || true).is_err());
    p.query("n:5", &mut || true).unwrap();
    assert!(!p.safe_mode());
    assert!(f.starts().iter().all(|s| s["safe"] == Value::Null));
}

#[test]
fn orderly_exits_are_not_crashes() {
    // The worker refusing to start (low memory) exits with a status, not a signal.
    let f = fake("");
    let mut p = MlxEmbedder::with_state(&f.models, Some(f.models.join("state")));
    for i in 0..3 { assert!(p.query(&format!("exit:{i}"), &mut || true).is_err()); }
    assert!(!p.safe_mode());
    p.query("n:1", &mut || true).unwrap();
    assert!(f.starts().iter().all(|s| s["safe"] == Value::Null));
}

#[test]
fn a_poison_item_counts_once_toward_safe_mode() {
    // The crashed batch counts; the culprit's single retry crashing again does not.
    let f = fake("");
    let mut p = MlxEmbedder::with_state(&f.models, Some(f.models.join("state")));
    let items = vec![Ok(vec![Input::text("n:1", false)]), Ok(vec![Input::text("crash:2", false)]), Ok(vec![Input::text("n:3", false)])];
    let out = p.embed_inputs_many(items, &mut || true);
    assert_eq!(hot(out[0].as_ref().unwrap()), 1);
    assert!(out[1].is_err());
    assert_eq!(hot(out[2].as_ref().unwrap()), 3);
    assert!(!p.safe_mode());
}

#[test]
fn trim_is_sent_once_per_idle_stretch() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    // Nothing running: nothing to trim, and no worker is started for it.
    assert!(!p.trim_if_idle(Duration::ZERO));
    assert!(f.starts().is_empty());
    p.query("n:1", &mut || true).unwrap();
    assert!(!p.trim_if_idle(Duration::from_secs(3600)));
    assert!(p.trim_if_idle(Duration::ZERO));
    assert!(!p.trim_if_idle(Duration::ZERO));
    assert_eq!(f.ops(), ["embed", "trim"]);
    // New use re-arms one more trim; the process stayed resident throughout.
    p.query("n:2", &mut || true).unwrap();
    assert!(p.trim_if_idle(Duration::ZERO));
    assert_eq!(f.ops(), ["embed", "trim", "embed", "trim"]);
    assert_eq!(f.starts().len(), 1);
}

// ---- grouping in the indexer, with a scripted provider ----

const S: u64 = 1_000_000_000;
const WALL: u64 = 1_800_000_000_000_000_000;
struct Grouping { groups: Vec<Vec<String>>, size: usize }
impl Embedder for Grouping {
    fn model(&self) -> Model { Model { id: "group-test".into(), version: "1".into(), dimensions: 2 } }
    fn embed(&mut self, _: &Path, c: &Chunk) -> Result<Vec<f32>> {
        self.groups.push(vec![c.kind.clone()]);
        if c.text.contains("broken") { anyhow::bail!("broken item") }
        Ok(vec![1., 0.])
    }
    fn batch_size(&self, _: &str) -> usize { self.size }
    fn embed_many_while(&mut self, _: &Path, chunks: &[Chunk], _: &mut dyn FnMut() -> bool) -> Vec<Result<Vec<f32>>> {
        self.groups.push(chunks.iter().map(|c| c.kind.clone()).collect());
        chunks.iter().map(|c| if c.text.contains("broken") { Err(anyhow::anyhow!("broken item")) } else { Ok(vec![1., 0.]) }).collect()
    }
}
#[test]
fn indexer_groups_same_kind_chunks_one_group_per_limit_and_isolates_failures() -> Result<()> {
    let t = tempfile::tempdir()?;
    let db = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-group"))?;
    let sid = j.session_id();
    store::create_segment(&db, sid, "capture", WALL, 0, "native")?;
    for i in 0..7u64 {
        let (start, end) = (S + i * 3 * S, 2 * S + i * 3 * S);
        let text = if i == 2 { "this one is broken" } else { "plain words here" };
        db.execute("INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,speaker,start_ns,end_ns,text,words) VALUES(?1,?2,?3,'mic','User',?4,?5,?6,'[]')",
            params![sid, format!("u{i}"), ((WALL + end) / 1_000_000) as i64, start as i64, end as i64, text])?;
        timeline::utterance(&db, sid, &format!("u{i}"))?;
    }
    let total: i64 = db.query_row("SELECT count(*) FROM timeline_chunks", [], |r| r.get(0))?;
    let broken: i64 = db.query_row("SELECT count(*) FROM timeline_chunks WHERE text LIKE '%broken%'", [], |r| r.get(0))?;
    assert!(total > 3 && broken >= 1);
    let mut p = Grouping { groups: vec![], size: 3 };
    let mut on = || BackfillGate { on_ac: true, idle_seconds: 300. };
    // `limit` counts groups: one provider call of at most three same-kind chunks.
    let first = timeline::embed_batch(t.path(), &mut p, 1, &mut on)?;
    assert_eq!(p.groups.len(), 1);
    assert!((1..=3).contains(&first));
    // Each call makes at most one group; stop once a call finds nothing left.
    for _ in 0..100 {
        let before = p.groups.len();
        timeline::embed_batch(t.path(), &mut p, 1, &mut on)?;
        assert!(p.groups.len() <= before + 1);
        if p.groups.len() == before { break; }
    }
    assert!(p.groups.iter().all(|g| !g.is_empty() && g.len() <= 3 && g.iter().all(|k| *k == g[0])), "{:?}", p.groups);
    assert!(p.groups.iter().any(|g| g.len() > 1));
    let embedded: i64 = db.query_row("SELECT count(*) FROM chunk_embeddings", [], |r| r.get(0))?;
    let errors: i64 = db.query_row("SELECT count(*) FROM chunk_embedding_errors", [], |r| r.get(0))?;
    // Only the broken chunks failed; their group neighbours were published.
    assert_eq!(embedded, total - broken);
    assert_eq!(errors, broken);
    Ok(())
}

#[test]
fn indexer_without_grouping_keeps_one_chunk_per_call() -> Result<()> {
    let t = tempfile::tempdir()?;
    let db = store::open(t.path())?;
    let j = Journal::open(&t.path().join("session-single"))?;
    let sid = j.session_id();
    store::create_segment(&db, sid, "capture", WALL, 0, "native")?;
    for i in 0..3u64 {
        let (start, end) = (S + i * 3 * S, 2 * S + i * 3 * S);
        db.execute("INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,speaker,start_ns,end_ns,text,words) VALUES(?1,?2,?3,'mic','User',?4,?5,'words','[]')",
            params![sid, format!("u{i}"), ((WALL + end) / 1_000_000) as i64, start as i64, end as i64])?;
        timeline::utterance(&db, sid, &format!("u{i}"))?;
    }
    let mut p = Grouping { groups: vec![], size: 1 };
    let n = timeline::embed_batch(t.path(), &mut p, 100, &mut || BackfillGate { on_ac: true, idle_seconds: 300. })?;
    assert!(n > 1);
    assert!(p.groups.iter().all(|g| g.len() == 1));
    assert_eq!(p.groups.len(), n);
    Ok(())
}

fn cancel_after(ms: u64) -> impl FnMut() -> bool {
    let started = Instant::now();
    move || started.elapsed() < Duration::from_millis(ms)
}

/// A caller's cancel while the worker is still starting (self-test, weight load) drops only
/// that request: the same process keeps starting, its stale reply is discarded, and the next
/// request gets its own answer from it. Nothing is started twice.
#[test]
fn cancel_while_starting_keeps_the_worker_and_drops_only_that_request() {
    let f = fake("");
    std::fs::write(f.native.join("startup.txt"), "0.6").unwrap();
    let mut p = MlxEmbedder::new(&f.models);
    let error = p.query("n:1", &mut cancel_after(50)).unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(p.running(), "a starting worker survives its caller's cancel");
    let pid = p.pid();
    assert_eq!(hot(&p.query("n:2", &mut || true).unwrap().values), 2);
    assert_eq!(p.pid(), pid);
    assert_eq!(f.starts().len(), 1);
    assert_eq!(f.ops(), ["embed", "embed"]);
}

/// Backlog indexing cancels every time the user touches the Mac, and a short-budget search can
/// cancel too: however often that happens during start-up, the start is never repeated.
#[test]
fn repeated_cancels_during_start_never_restart_it() {
    let f = fake("");
    std::fs::write(f.native.join("startup.txt"), "0.8").unwrap();
    let mut p = MlxEmbedder::new(&f.models);
    for k in 1..=3 {
        assert!(p.query(&format!("n:{k}"), &mut cancel_after(40)).is_err());
        assert!(p.running());
    }
    // A cancelled batch is abandoned the same way.
    let out = p.embed_inputs_many(vec![input("n:4"), input("n:5")], &mut cancel_after(40));
    assert!(out.iter().all(|r| r.is_err()));
    assert!(p.running());
    assert_eq!(hot(&p.query("n:6", &mut || true).unwrap().values), 6);
    assert_eq!(f.starts().len(), 1);
    assert_eq!(f.ops(), ["embed", "embed", "embed", "batch", "embed"]);
    // Up and answering: the ordinary rules apply again.
    assert_eq!(hot(&p.query("n:7", &mut || true).unwrap().values), 7);
}

/// Once the worker has answered, a cancel still ends it (a long batch must not keep the GPU busy).
#[test]
fn cancel_of_a_started_worker_still_stops_it() {
    let f = fake("");
    let mut p = MlxEmbedder::new(&f.models);
    assert_eq!(hot(&p.query("n:1", &mut || true).unwrap().values), 1);
    assert!(p.query("slow:2", &mut cancel_after(50)).unwrap_err().to_string().contains("cancelled"));
    assert!(!p.running());
    assert_eq!(hot(&p.query("n:3", &mut || true).unwrap().values), 3);
    assert_eq!(f.starts().len(), 2);
}

/// A worker that is still starting, or still answering a request abandoned during its start,
/// is not idle: trim waits for it without blocking, then runs once.
#[test]
fn trim_waits_for_a_start_abandoned_by_its_caller() {
    let f = fake("");
    std::fs::write(f.native.join("startup.txt"), "0.5").unwrap();
    let mut p = MlxEmbedder::new(&f.models);
    assert!(p.query("n:1", &mut cancel_after(30)).is_err());
    let started = Instant::now();
    assert!(!p.trim_if_idle(Duration::ZERO));
    assert!(started.elapsed() < Duration::from_millis(200), "trim must not wait on a starting worker");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !p.trim_if_idle(Duration::ZERO) {
        assert!(Instant::now() < deadline, "trim never ran after the start finished");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(p.running());
    assert_eq!(f.ops(), ["embed", "trim"]);
    assert_eq!(f.starts().len(), 1);
}
