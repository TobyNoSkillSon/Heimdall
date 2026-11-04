//! The climb's metric (plan.md section 2): median warm decision time in ms, end to end in the engine (state and
//! question strings in, probabilities out), over 20 test rows of 1,800 to 2,100 tokens after 5 warm-up calls.
//! Run only with the GPU idle (training paused) and the Mac otherwise quiet.
//!
//!   bench --checkpoint DIR [--rows check/guard_rows.jsonl] [--n 20] [--warmup 5] [--min-tokens 1800]
//!         [--max-tokens 2100] [--max-used-gb 89] [--expect-gb 5] [--release 0|1]

use anyhow::{bail, Context, Result};
use d1v5_engine::{mem, Engine};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Instant;

fn main() -> Result<()> {
    let mut flags = HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        flags.insert(k.trim_start_matches("--").to_string(), it.next().context("flag without a value")?);
    }
    let get = |k: &str, d: &str| flags.get(k).cloned().unwrap_or_else(|| d.to_string());
    let dir = flags.get("checkpoint").context("--checkpoint is required")?.clone();
    let rows_path = get("rows", concat!(env!("CARGO_MANIFEST_DIR"), "/check/guard_rows.jsonl"));
    let (n, warm): (usize, usize) = (get("n", "20").parse()?, get("warmup", "5").parse()?);
    let (lo, hi): (usize, usize) = (get("min-tokens", "1800").parse()?, get("max-tokens", "2100").parse()?);
    let rows: Vec<Value> = std::fs::read_to_string(&rows_path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let picked: Vec<&Value> = rows
        .iter()
        .filter(|r| r["ids"].as_array().map(|a| (lo..=hi).contains(&a.len())).unwrap_or(false))
        .take(n)
        .collect();
    if picked.len() < n {
        bail!("only {} rows of {lo}..{hi} tokens", picked.len());
    }
    let used = mem::used_gb()?;
    let (max_used, expect): (f64, f64) = (get("max-used-gb", "89").parse()?, get("expect-gb", "5").parse()?);
    if used + expect >= max_used {
        bail!("memory: {used:.1} GB used + {expect} GB expected would pass {max_used} GB");
    }
    let _ = mlx_rs::memory::reset_peak_memory();
    let engine = Engine::load(&dir)?;
    let gb = |b: usize| (b as f64 / 1e9 * 1e4).round() / 1e4;
    // after the load (its transforms' freed buffers already released): the resident weights and kernels
    let load_peak = gb(mlx_rs::memory::peak_memory().unwrap_or(0));
    let active_after_load = gb(mlx_rs::memory::active_memory().unwrap_or(0));
    let footprint_after_load = gb(phys_footprint() as usize);
    // --release 1: MLX's cache cleared after every decision, inside the timing, as the C ABI does after every request
    // (compare precisions this way: a quantized forward clears its own cache, a BF16 one does not)
    let release = get("release", "0") == "1";
    let call = |r: &Value| -> Result<()> {
        let q = serde_json::to_string(&r["question"])?;
        engine.decide(r["state"].as_str().context("state")?, &q)?;
        if release {
            d1v5_engine::cache::release();
        }
        Ok(())
    };
    for r in picked.iter().cycle().take(warm) {
        call(r)?;
    }
    let _ = mlx_rs::memory::reset_peak_memory();
    let mut ms = Vec::with_capacity(n);
    let mut parts: Vec<[f64; 4]> = vec![];
    for r in &picked {
        let t0 = Instant::now();
        if std::env::var("D1E_PROFILE").is_ok_and(|v| v == "1") {
            let q = d1v5_engine::Question::from_json(&r["question"])?;
            let enc = engine.encode(r["state"].as_str().context("state")?, &q)?;
            let te = t0.elapsed().as_secs_f64() * 1e3;
            let s = engine.scores(&enc)?;
            let _ = d1v5_engine::softmax(&s, engine.calibration.temperature(&q));
            let p = engine.profile().unwrap_or_default();
            parts.push([te, p[0], p[1], p[2]]);
        } else {
            call(r)?;
        }
        ms.push(t0.elapsed().as_secs_f64() * 1e3);
    }
    let med = |mut v: Vec<f64>| { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); let n = v.len(); if n == 0 { 0.0 } else if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 } };
    let profile = (!parts.is_empty()).then(|| json!({
        "render_tokenize_ms": med(parts.iter().map(|p| p[0]).collect()),
        "graph_build_launch_ms": med(parts.iter().map(|p| p[1]).collect()),
        "trunk_eval_ms": med(parts.iter().map(|p| p[2]).collect()),
        "readout_ms": med(parts.iter().map(|p| p[3]).collect())}));
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 };
    println!("{}", json!({"requested": engine.requested.name(), "backend": engine.backend().name(),
                           "options": engine.backend().opts().names(),
                           "state_tokens": engine.state_tokens, "skip_layers": std::env::var("D1E_SKIP_LAYERS").ok(), "tensor_mm": std::env::var("D1E_TENSOR_MM").ok(), "pad_multiple": std::env::var("D1E_PAD_MULTIPLE").ok(), "option_notes": engine.notes(), "rows": n, "warmup": warm, "median_ms": median,
                           "profile": profile, "min_ms": sorted[0], "max_ms": sorted[n - 1], "ms": ms,
                           "precision": engine.precision(),
                           "memory_gb": {"mlx_peak_during_load": load_peak, "mlx_active_after_load": active_after_load,
                                         "mlx_peak_during_decisions": gb(mlx_rs::memory::peak_memory().unwrap_or(0)),
                                         "mlx_active_after_decisions": gb(mlx_rs::memory::active_memory().unwrap_or(0)),
                                         "phys_footprint_after_load": footprint_after_load,
                                         "phys_footprint_after_decisions": gb(phys_footprint() as usize),
                                         "phys_footprint_lifetime_max": gb(phys_footprint_max() as usize)},
                           "tokens": picked.iter().map(|r| r["ids"].as_array().map_or(0, |a| a.len())).collect::<Vec<_>>()}));
    Ok(())
}

fn phys_footprint() -> u64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4,
                              &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    if rc == 0 { info.ri_phys_footprint } else { 0 }
}

/// The process's largest phys_footprint so far (load and decisions included).
fn phys_footprint_max() -> u64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4,
                              &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    if rc == 0 { info.ri_lifetime_max_phys_footprint } else { 0 }
}
