//! The engine's guards (plan.md section 4).
//!
//!   check ids   --checkpoint DIR [--rows check/guard_rows.jsonl] [--specials check/specials.json]
//!       Prompt guard, no weights: the engine's token ids, answer slot, option names and token groups equal
//!       prep.py's (d1-2B's own runner) on every guard row; the special-token list equals the Python tokenizer's.
//!   check probs --checkpoint DIR --ref check/ref_<name>.jsonl [--rows check/guard_rows.jsonl] [--out FILE]
//!       BF16 guard: for every row in the reference file, the engine (state and question strings in) gives the
//!       same argmax and max |p - p_ref| <= 0.03 at the checkpoint's temperature. Refuses to load the weights
//!       unless the Mac's used memory plus --expect-gb (5) stays under --max-used-gb (89).
//!
//!   check abi   --checkpoint DIR [--dylib target/release/libd1v5_engine.dylib]
//!       The C ABI through dlopen of the built dylib: version, create, tokens, decide (a guard row's dispatch
//!       question and a noul in one request), free, destroy; the reply in the shape Heimdall's d1.rs parses
//!       (output_tokens 0, probabilities in [0, 1] summing to 1, the choice their argmax), and the choice's
//!       probabilities equal to Engine::decide's.
//!
//!   With --plain FILE (a `check probs --out` file from the plain path), each row must also match the plain path:
//!   same argmax and max |dp| <= 0.03 (plan.md's guard); the raw-score difference is reported only.
//!
//! Exit status 0 only when every row passes.

use anyhow::{bail, Context, Result};
use d1v5_engine::{mem, softmax, Backend, Engine, Prompt, Question};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

const TOL: f32 = 0.03;
const GB: f64 = (1u64 << 30) as f64;

struct Args {
    mode: String,
    flags: HashMap<String, String>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut it = std::env::args().skip(1);
        let mode = it.next().context("usage: check ids|probs --checkpoint DIR ...")?;
        let mut flags = HashMap::new();
        while let Some(k) = it.next() {
            let k = k.strip_prefix("--").with_context(|| format!("unexpected {k}"))?.to_string();
            flags.insert(k, it.next().context("flag without a value")?);
        }
        Ok(Self { mode, flags })
    }
    fn get(&self, k: &str, default: &str) -> String {
        self.flags.get(k).cloned().unwrap_or_else(|| default.to_string())
    }
    fn need(&self, k: &str) -> Result<String> {
        self.flags.get(k).cloned().with_context(|| format!("--{k} is required"))
    }
}

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_jsonl(path: &str) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| path.to_string())?;
    text.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

fn u32s(v: &Value) -> Vec<u32> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as u32).collect()).unwrap_or_default()
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0;
    for (i, x) in v.iter().enumerate() {
        if *x > v[best] {
            best = i;
        }
    }
    best
}

fn ids_guard(a: &Args) -> Result<bool> {
    let dir = PathBuf::from(a.need("checkpoint")?);
    let rows = read_jsonl(&a.get("rows", here().join("check/guard_rows.jsonl").to_str().unwrap()))?;
    let prompt = Prompt::load(&dir)?;
    let mut ok = true;

    let specials_path = a.get("specials", here().join("check/specials.json").to_str().unwrap());
    let sp: Value = serde_json::from_str(&std::fs::read_to_string(&specials_path)?)?;
    let mut want: Vec<String> = sp["specials"].as_array().context("specials")?.iter()
        .filter_map(Value::as_str).map(String::from).collect();
    let mut got = prompt.special_names();
    want.sort();
    got.sort();
    let specials_equal = want == got;
    ok &= specials_equal;

    let (mut equal, mut first_bad) = (0usize, Vec::new());
    let mut per_task: HashMap<String, (usize, usize)> = HashMap::new();
    for r in &rows {
        let q = Question::from_json(&r["question"])?;
        let enc = prompt.encode(r["state"].as_str().context("state")?, &q)?;
        let groups: Vec<Vec<u32>> = r["groups"].as_array().context("groups")?.iter().map(u32s).collect();
        let names: Vec<String> = r["names"].as_array().context("names")?.iter()
            .filter_map(Value::as_str).map(String::from).collect();
        let same = enc.ids == u32s(&r["ids"]) && enc.slot as u64 == r["slot"].as_u64().unwrap_or(u64::MAX)
            && enc.groups == groups && enc.names == names;
        let t = per_task.entry(r["task"].as_str().unwrap_or("?").to_string()).or_default();
        t.1 += 1;
        if same {
            equal += 1;
            t.0 += 1;
        } else if first_bad.len() < 5 {
            let want = u32s(&r["ids"]);
            let at = enc.ids.iter().zip(&want).position(|(x, y)| x != y).unwrap_or(enc.ids.len().min(want.len()));
            first_bad.push(json!({"id": r["id"], "first_diff_at": at, "len": enc.ids.len(), "want_len": want.len(),
                                  "groups_equal": enc.groups == groups, "names_equal": enc.names == names}));
        }
    }
    ok &= equal == rows.len();
    println!("{}", json!({"guard": "ids", "checkpoint": dir, "rows": rows.len(), "equal": equal,
                           "per_task": per_task, "specials_equal": specials_equal, "specials": got.len(),
                           "mismatches": first_bad, "pass": ok}));
    Ok(ok)
}

fn probs_guard(a: &Args) -> Result<bool> {
    let dir = PathBuf::from(a.need("checkpoint")?);
    let refs = read_jsonl(&a.need("ref")?)?;
    let rows = read_jsonl(&a.get("rows", here().join("check/guard_rows.jsonl").to_str().unwrap()))?;
    let by_id: HashMap<&str, &Value> = rows.iter().map(|r| (r["id"].as_str().unwrap_or(""), r)).collect();

    let max_used: f64 = a.get("max-used-gb", "89").parse()?;
    let expect: f64 = a.get("expect-gb", "5").parse()?;
    let used = mem::used_gb()?;
    if used + expect >= max_used {
        bail!("memory: {used:.1} GB used + {expect} GB expected would pass {max_used} GB; not loading the weights");
    }
    // keep freed MLX buffers from piling up in the cache while training shares the Mac (no effect on the numbers)
    let cache_gb: f64 = a.get("cache-limit-gb", "1").parse()?;
    mlx_rs::memory::set_cache_limit((cache_gb * GB) as usize).map_err(|e| anyhow::anyhow!("{e}"))?;
    let engine = Engine::load(&dir)?;
    // optional: the plain path's own results (a `check probs --out` file run with every option off)
    let plain: HashMap<String, Value> = match a.flags.get("plain") {
        None => HashMap::new(),
        Some(path) => {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
            v["rows"].as_array().context("plain rows")?.iter()
                .map(|r| (r["id"].as_str().unwrap_or("").to_string(), r.clone())).collect()
        }
    };
    let mut out = Vec::new();
    let (mut pass_rows, mut argmax_same, mut worst, mut worst_t1, mut worst_score) = (0, 0, 0f32, 0f32, 0f32);
    let (mut p_argmax, mut p_dp, mut p_ds, mut p_rows) = (0usize, 0f32, 0f32, 0usize);
    let mut peak_used = used;
    for r in &refs {
        let id = r["id"].as_str().context("ref id")?;
        let row = by_id.get(id).with_context(|| format!("{id}: not in the guard rows"))?;
        let q = Question::from_json(&row["question"])?;
        let enc = engine.encode(row["state"].as_str().context("state")?, &q)?;
        let scores = engine.scores(&enc)?;
        let t = engine.calibration.temperature(&q);
        let p = softmax(&scores, t);
        let p1 = softmax(&scores, 1.0);
        let pr: Vec<f32> = r["probs"].as_array().context("ref probs")?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
        let pr1: Vec<f32> = r["probs_t1"].as_array().context("ref probs_t1")?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
        let sr: Vec<f32> = r["scores"].as_array().context("ref scores")?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
        if pr.len() != p.len() {
            bail!("{id}: {} options here, {} in the reference", p.len(), pr.len());
        }
        let d = p.iter().zip(&pr).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
        let d1 = p1.iter().zip(&pr1).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
        let ds = scores.iter().zip(&sr).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
        let same = argmax(&p) == argmax(&pr);
        let mut pass = same && d <= TOL;
        let mut vs_plain = Value::Null;
        if let Some(pr) = plain.get(id) {
            let pp: Vec<f32> = pr["probs"].as_array().context("plain probs")?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
            let ps: Vec<f32> = pr["scores"].as_array().context("plain scores")?.iter().map(|x| x.as_f64().unwrap() as f32).collect();
            let dp = p.iter().zip(&pp).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
            let dsp = scores.iter().zip(&ps).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
            let am = argmax(&p) == argmax(&pp);
            p_rows += 1;
            p_argmax += am as usize;
            p_dp = p_dp.max(dp);
            p_ds = p_ds.max(dsp);
            pass &= am && dp <= TOL; // the raw-score difference is reported, not judged (BF16 steps)
            vs_plain = json!({"argmax_same": am, "max_abs_diff": dp, "max_score_diff": dsp});
        }
        pass_rows += pass as usize;
        argmax_same += same as usize;
        worst = worst.max(d);
        worst_t1 = worst_t1.max(d1);
        worst_score = worst_score.max(ds);
        peak_used = peak_used.max(mem::used_gb().unwrap_or(0.0));
        out.push(json!({"id": id, "task": row["task"], "tokens": enc.ids.len(), "temperature": t,
                        "argmax_same": same, "max_abs_diff": d, "max_abs_diff_t1": d1, "max_score_diff": ds,
                        "probs": p, "probs_ref": pr, "scores": scores, "scores_ref": sr, "vs_plain": vs_plain,
                        "pass": pass}));
    }
    let ok = pass_rows == refs.len() && !refs.is_empty();
    let summary = json!({"guard": "probs", "checkpoint": dir, "requested": engine.requested.name(),
                         "backend": engine.backend().name(), "option_notes": engine.notes(),
                         "vs_plain": if p_rows > 0 { json!({"rows": p_rows, "argmax_same": p_argmax,
                             "max_abs_diff": p_dp, "max_score_diff": p_ds}) } else { Value::Null },
                         "fallback_forced": d1v5_engine::fallback_forced(), "rows": refs.len(), "pass_rows": pass_rows,
                         "argmax_same": argmax_same, "max_abs_diff": worst, "max_abs_diff_t1": worst_t1,
                         "max_score_diff": worst_score, "tolerance": TOL,
                         "temperature": engine.calibration.default, "mem_used_gb_before": used,
                         "mem_used_gb_peak_seen": peak_used,
                         "max_activation": engine.max_activation(),
                         "mlx_peak_gb": mlx_rs::memory::peak_memory().unwrap_or(0) as f64 / GB,
                         "mlx_cache_gb": mlx_rs::memory::cache_memory().unwrap_or(0) as f64 / GB, "pass": ok});
    if let Some(path) = a.flags.get("out") {
        std::fs::write(path, serde_json::to_string_pretty(&json!({"summary": summary, "rows": out}))? + "\n")?;
    }
    println!("{summary}");
    Ok(ok)
}

fn abi_check(a: &Args) -> Result<bool> {
    use std::ffi::{c_char, c_void, CStr, CString};
    type Create = unsafe extern "C" fn(*const c_char, *const c_char, i32, bool, *mut *mut c_char) -> *mut c_void;
    type Decide = unsafe extern "C" fn(*mut c_void, *const c_char, i32, *mut *mut c_char) -> *mut c_char;
    type Tokens = unsafe extern "C" fn(*mut c_void, *const c_char) -> i32;
    type Free = unsafe extern "C" fn(*mut c_char);
    type Destroy = unsafe extern "C" fn(*mut c_void);
    type Version = unsafe extern "C" fn() -> *const c_char;

    let dir = a.need("checkpoint")?;
    let dylib = a.get("dylib", here().join("target/release/libd1v5_engine.dylib").to_str().unwrap());
    let rows = read_jsonl(&a.get("rows", here().join("check/guard_rows.jsonl").to_str().unwrap()))?;
    let row = rows.iter().find(|r| r["task"] == "dispatch").context("a dispatch row")?;
    let used = mem::used_gb()?;
    if used + 5.0 >= a.get("max-used-gb", "89").parse::<f64>()? {
        bail!("memory: {used:.1} GB used; not loading the weights");
    }
    mlx_rs::memory::set_cache_limit(1 << 30).map_err(|e| anyhow::anyhow!("{e}"))?;
    let noul = json!({"type": "noul", "instructions": "Is the pending thought complete enough to send now?"});
    let request = json!({"state": row["state"], "questions": {"dispatch": row["question"], "send_now": noul}});
    let lib = CString::new(dylib.clone())?;
    let h = unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        bail!("dlopen {dylib} failed");
    }
    let sym = |name: &str| -> Result<*mut c_void> {
        let n = CString::new(name)?;
        let p = unsafe { libc::dlsym(h, n.as_ptr()) };
        if p.is_null() { bail!("missing symbol {name}") } else { Ok(p) }
    };
    let (create, decide, tokens, free, destroy, version) = unsafe {
        (std::mem::transmute::<_, Create>(sym("heimdall_d1_create_text_backend")?),
         std::mem::transmute::<_, Decide>(sym("heimdall_d1_decide")?),
         std::mem::transmute::<_, Tokens>(sym("heimdall_d1_tokens")?),
         std::mem::transmute::<_, Free>(sym("heimdall_d1_free")?),
         std::mem::transmute::<_, Destroy>(sym("heimdall_d1_destroy")?),
         std::mem::transmute::<_, Version>(sym("heimdall_d1_version")?))
    };
    sym("heimdall_d1_decide_interruptible")?;
    sym("heimdall_d1_recreate_device")?;
    let ver = unsafe { CStr::from_ptr(version()) }.to_string_lossy().into_owned();
    let model = CString::new(dir.clone())?;
    let empty = CString::new("")?;
    let mut err: *mut c_char = std::ptr::null_mut();
    let handle = unsafe { create(model.as_ptr(), empty.as_ptr(), 2, true, &mut err) };
    if handle.is_null() {
        let m = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        bail!("create failed: {m}");
    }
    let text = CString::new(row["state"].as_str().unwrap())?;
    let n_state = unsafe { tokens(handle, text.as_ptr()) };
    let body = CString::new(request.to_string())?;
    let raw = unsafe { decide(handle, body.as_ptr(), 30000, &mut err) };
    if raw.is_null() {
        let m = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
        unsafe { free(err) };
        bail!("decide failed: {m}");
    }
    let reply: Value = serde_json::from_str(&unsafe { CStr::from_ptr(raw) }.to_string_lossy())?;
    unsafe { free(raw) };
    // a bad request must come back as an error string, not a crash
    let bad = CString::new(r#"{"state": "x", "questions": {"q": {"type": "score", "instructions": "?"}}}"#)?;
    let mut err2: *mut c_char = std::ptr::null_mut();
    let none = unsafe { decide(handle, bad.as_ptr(), 30000, &mut err2) };
    let bad_refused = none.is_null() && !err2.is_null();
    if !err2.is_null() {
        unsafe { free(err2) };
    }
    // heimdall_d1_test_fail_next: the next decide fails with the GPU-fault marker, the one after works again
    type FailNext = unsafe extern "C" fn(*mut c_void);
    let fail_next = unsafe { std::mem::transmute::<_, FailNext>(sym("heimdall_d1_test_fail_next")?) };
    unsafe { fail_next(handle) };
    let mut err3: *mut c_char = std::ptr::null_mut();
    let small = CString::new(json!({"state": "x", "questions": {"q": noul}}).to_string())?;
    let r3 = unsafe { decide(handle, small.as_ptr(), 30000, &mut err3) };
    let fault_marked = r3.is_null() && !err3.is_null()
        && unsafe { CStr::from_ptr(err3) }.to_string_lossy().contains("HEIMDALL_GPU_FAULT");
    if !err3.is_null() {
        unsafe { free(err3) };
    }
    let r4 = unsafe { decide(handle, small.as_ptr(), 30000, &mut err3) };
    let recovered = !r4.is_null();
    if recovered {
        unsafe { free(r4) };
    }
    unsafe { destroy(handle) };

    let mut ok = reply["usage"]["output_tokens"] == 0 && reply["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0;
    ok &= bad_refused && n_state > 0 && fault_marked && recovered;
    for key in ["dispatch", "send_now"] {
        let probs = reply["answers"][key]["probabilities"].as_object().context("probabilities")?;
        let sum: f64 = probs.values().filter_map(Value::as_f64).sum();
        ok &= (sum - 1.0).abs() < 1e-4 && probs.values().all(|p| (0.0..=1.0).contains(&p.as_f64().unwrap_or(-1.0)));
    }
    let probs = reply["answers"]["dispatch"]["probabilities"].as_object().unwrap();
    let best = probs.iter().max_by(|x, y| x.1.as_f64().partial_cmp(&y.1.as_f64()).unwrap()).unwrap().0.clone();
    ok &= reply["answers"]["dispatch"]["choice"] == best.as_str();
    ok &= reply["answers"]["send_now"]["noul"].as_f64().is_some();
    ok &= reply["precision"].as_str().is_some_and(|p| ["bf16", "fp16", "int8", "int4", "mixed"].contains(&p));
    // the same question through the Rust API
    let engine = Engine::load(&dir)?;
    let direct = engine.decide(row["state"].as_str().unwrap(), &row["question"].to_string())?;
    let diff = direct.iter().map(|(n, p)| (p - probs[n].as_f64().unwrap() as f32).abs()).fold(0f32, f32::max);
    ok &= diff == 0.0;
    println!("{}", json!({"guard": "abi", "version": ver, "precision": reply["precision"], "backend": reply["backend"],
                           "state_tokens": n_state, "usage": reply["usage"],
                           "dispatch_choice": reply["answers"]["dispatch"]["choice"],
                           "send_now": reply["answers"]["send_now"]["noul"], "bad_request_refused": bad_refused,
                           "test_fail_next_marked": fault_marked, "recovered_after": recovered,
                           "rust_api_max_diff": diff, "pass": ok}));
    Ok(ok)
}

/// Every guard row's calibrated probabilities on the engine as the environment configures it, one JSON line a row,
/// for check/agree.py.
fn decide_rows(a: &Args) -> Result<bool> {
    let dir = PathBuf::from(a.need("checkpoint")?);
    let rows = read_jsonl(&a.get("rows", here().join("check/guard_rows.jsonl").to_str().unwrap()))?;
    let out_path = a.need("out")?;
    let used = mem::used_gb()?;
    if used + 5.0 >= a.get("max-used-gb", "89").parse::<f64>()? {
        bail!("memory: {used:.1} GB used; not loading the weights");
    }
    mlx_rs::memory::set_cache_limit(1 << 30).map_err(|e| anyhow::anyhow!("{e}"))?;
    let engine = Engine::load(&dir)?;
    let mut lines = String::new();
    for r in &rows {
        let q = Question::from_json(&r["question"])?;
        let enc = engine.encode(r["state"].as_str().context("state")?, &q)?;
        let scores = engine.scores(&enc)?;
        let t = engine.calibration.temperature(&q);
        let p = softmax(&scores, t);
        lines += &json!({"id": r["id"], "task": r["task"], "names": enc.names, "probs": p, "scores": scores,
                         "temperature": t, "tokens": enc.ids.len(),
                         "full_tokens": r["ids"].as_array().map(|x| x.len())}).to_string();
        lines.push('\n');
    }
    std::fs::write(&out_path, lines)?;
    println!("{}", json!({"mode": "decide", "rows": rows.len(), "backend": engine.backend().name(),
                           "precision": engine.precision(),
                           "mlx_active_gb": mlx_rs::memory::active_memory().unwrap_or(0) as f64 / GB,
                           "mlx_peak_gb": mlx_rs::memory::peak_memory().unwrap_or(0) as f64 / GB,
                           "state_tokens": engine.state_tokens, "skip_layers": std::env::var("D1E_SKIP_LAYERS").ok(),
                           "notes": engine.notes(), "out": out_path}));
    Ok(true)
}

fn main() -> Result<()> {
    let a = Args::parse()?;
    let _ = Backend::select();
    let ok = match a.mode.as_str() {
        "ids" => ids_guard(&a)?,
        "probs" => probs_guard(&a)?,
        "abi" => abi_check(&a)?,
        "decide" => decide_rows(&a)?,
        other => bail!("unknown mode {other}: ids or probs"),
    };
    std::process::exit(if ok { 0 } else { 1 });
}
