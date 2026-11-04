//! Interleaved timing inside one process, for a busy Mac: several engines loaded side by side, each from its own
//! checkpoint and environment, and the bench rows decided A, B, C, A, B, C, ... so slow drift (heat, other jobs) hits
//! every variant alike. Prints each variant's median, min and max ms per decision and its median paired ratio
//! against the first variant (same row, same cycle).
//!
//!   abbench --rows FILE [--cycles 3] [--warmup 10] [--n 20] [--min-tokens 1800] [--max-tokens 2100]
//!           [--release 1] [--max-used-gb 89] --v "name|CHECKPOINT|VAR=1 VAR2=0" --v ...
//!
//! Each variant's variables are set while it loads (options and kernel shapes are read at load), then removed.

use anyhow::{bail, Context, Result};
use d1v5_engine::{mem, Backend, Engine};
use serde_json::{json, Value};
use std::time::Instant;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn main() -> Result<()> {
    let mut specs = vec![];
    let mut flags = std::collections::HashMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().context("flag without a value")?;
        if k == "--v" { specs.push(v) } else { flags.insert(k.trim_start_matches("--").to_string(), v); }
    }
    let get = |k: &str, d: &str| flags.get(k).cloned().unwrap_or_else(|| d.to_string());
    let (cycles, warm, n): (usize, usize, usize) = (get("cycles", "3").parse()?, get("warmup", "10").parse()?, get("n", "20").parse()?);
    let (lo, hi): (usize, usize) = (get("min-tokens", "1800").parse()?, get("max-tokens", "2100").parse()?);
    let rows: Vec<Value> = std::fs::read_to_string(flags.get("rows").context("--rows")?)?
        .lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
    let picked: Vec<&Value> = rows.iter()
        .filter(|r| r["ids"].as_array().map(|a| (lo..=hi).contains(&a.len())).unwrap_or(false)).take(n).collect();
    if picked.len() < n || specs.is_empty() {
        bail!("{} rows of {lo}..{hi} tokens, {} variants", picked.len(), specs.len());
    }
    let max_used: f64 = get("max-used-gb", "89").parse()?;
    let mut engines = vec![];
    for s in &specs {
        let parts: Vec<&str> = s.splitn(3, '|').collect();
        let [name, dir, envs] = parts[..] else { bail!("--v \"name|CHECKPOINT|VARS\": {s}") };
        let used = mem::used_gb()?;
        if used + 5.0 >= max_used {
            bail!("memory: {used:.1} GB used; not loading {name}");
        }
        let vars: Vec<(String, String)> = envs.split_whitespace()
            .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
        // these are read at run time or are process-wide, so one variant cannot hold its own value
        if let Some((k, _)) = vars.iter().find(|(k, _)| ["D1E_PAD_MULTIPLE", "D1E_CACHE_LIMIT_MB", "D1E_PROFILE",
                                                         "D1E_TEST_FAIL_FAST", "MLX_BFS_MAX_WIDTH"].contains(&k.as_str())) {
            bail!("{k} cannot differ between variants in one process; use abtime (separate processes)");
        }
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        let e = Engine::load_with(dir, Backend::select());
        for (k, _) in &vars {
            std::env::remove_var(k);
        }
        let e = e?;
        eprintln!("{name}: {} {} notes {:?}", e.precision(), e.backend().name(), e.notes());
        engines.push((name.to_string(), e));
    }
    // --release 1 (default): MLX's cache cleared after every decision, inside the timing, as Heimdall's C ABI does
    let release = get("release", "1") == "1";
    let call = |e: &Engine, r: &Value| -> Result<f64> {
        let q = serde_json::to_string(&r["question"])?;
        let t0 = Instant::now();
        e.decide(r["state"].as_str().context("state")?, &q)?;
        if release {
            d1v5_engine::cache::release(); // as the C ABI does after every request
        }
        Ok(t0.elapsed().as_secs_f64() * 1e3)
    };
    for r in picked.iter().cycle().take(warm) {
        for (_, e) in &engines {
            call(e, r)?;
        }
    }
    let mut ms: Vec<Vec<f64>> = vec![vec![]; engines.len()];
    for c in 0..cycles {
        for r in &picked {
            // rotate the order each cycle so no variant always runs first after another
            for k in 0..engines.len() {
                let i = (k + c) % engines.len();
                ms[i].push(call(&engines[i].1, r)?);
            }
        }
    }
    let out: Vec<Value> = engines.iter().enumerate().map(|(i, (name, e))| {
        let ratio = median(ms[i].iter().zip(&ms[0]).map(|(a, b)| a / b).collect());
        json!({"variant": name, "precision": e.precision(), "backend": e.backend().name(), "notes": e.notes(),
               "median_ms": median(ms[i].clone()), "min_ms": ms[i].iter().cloned().fold(f64::INFINITY, f64::min),
               "max_ms": ms[i].iter().cloned().fold(0.0, f64::max), "ratio_vs_first": ratio, "decisions": ms[i].len(),
               "cycle_medians": (0..cycles).map(|c| median(ms[i][c * n..(c + 1) * n].to_vec())).collect::<Vec<_>>()})
    }).collect();
    println!("{}", json!({"cycles": cycles, "rows": n, "warmup": warm, "release_each": release, "variants": out}));
    Ok(())
}
