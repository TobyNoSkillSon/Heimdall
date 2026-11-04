//! Resident memory of the engine in its own process, through the C ABI Heimdall calls (create, decide per row).
//! Loads the 2B model: opt-in, one run at a time, never inside the app.
//!
//!   memcheck burst   --checkpoint DIR --rows FILE [--n 30] --out FILE     the cache policy from D1E_CACHE_LIMIT_MB
//!   memcheck latency --checkpoint DIR --rows FILE [--n 20] [--warmup 3]   capped vs uncapped, interleaved per row
//!
//! `burst` writes each row's probabilities and decide time, then prints MLX's active and cached bytes, this process's
//! phys_footprint and `footprint`'s IOAccelerator (GPU buffers) dirty bytes, after load and after the burst.

use anyhow::{bail, Context, Result};
use d1v5_engine::{cache, ffi, mem};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::{c_char, CStr, CString};
use std::time::Instant;

fn phys_footprint() -> u64 {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V4,
                              &mut info as *mut _ as *mut libc::rusage_info_t)
    };
    if rc == 0 { info.ri_phys_footprint } else { 0 }
}

/// `footprint -f bytes`: (total footprint, IOAccelerator dirty bytes).
fn footprint() -> Result<(u64, u64)> {
    let out = std::process::Command::new("/usr/bin/footprint")
        .args(["-f", "bytes", "-p", &std::process::id().to_string()])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let total = text
        .split("Footprint: ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .context("footprint total")?;
    let gpu = text
        .lines()
        .filter(|l| l.contains("IOAccelerator"))
        .filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok())
        .sum();
    Ok((total, gpu))
}

fn snapshot(label: &str) -> Result<Value> {
    let (total, gpu) = footprint()?;
    let gb = |b: u64| (b as f64 / 1e9 * 1000.0).round() / 1000.0;
    Ok(json!({"at": label, "phys_footprint_gb": gb(phys_footprint()), "footprint_gb": gb(total),
              "ioaccelerator_gb": gb(gpu),
              "mlx_active_gb": gb(mlx_rs::memory::active_memory().unwrap_or(0) as u64),
              "mlx_cache_gb": gb(mlx_rs::memory::cache_memory().unwrap_or(0) as u64),
              "mlx_peak_gb": gb(mlx_rs::memory::peak_memory().unwrap_or(0) as u64)}))
}

struct Abi(*mut std::ffi::c_void);

impl Abi {
    fn load(dir: &str) -> Result<Self> {
        let path = CString::new(dir)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let h = ffi::heimdall_d1_create_text_backend(path.as_ptr(), std::ptr::null(), 0, true, &mut err);
        if h.is_null() {
            bail!("create: {}", unsafe { CStr::from_ptr(err) }.to_string_lossy());
        }
        Ok(Self(h))
    }

    fn decide(&self, request: &Value) -> Result<Value> {
        let body = CString::new(request.to_string())?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let out = ffi::heimdall_d1_decide(self.0, body.as_ptr(), 0, &mut err);
        if out.is_null() {
            bail!("decide: {}", unsafe { CStr::from_ptr(err) }.to_string_lossy());
        }
        let v = serde_json::from_str(&unsafe { CStr::from_ptr(out) }.to_string_lossy())?;
        ffi::heimdall_d1_free(out);
        Ok(v)
    }
}

fn requests(path: &str, n: usize) -> Result<Vec<(Value, Value)>> {
    let rows: Vec<Value> = std::fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let picked: Vec<(Value, Value)> = rows
        .into_iter()
        .take(n)
        .map(|r| (r["id"].clone(), json!({"state": r["state"], "questions": {"q": r["question"]}})))
        .collect();
    if picked.len() < n {
        bail!("only {} rows", picked.len());
    }
    Ok(picked)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().context("mode: burst or latency")?;
    let mut flags = HashMap::new();
    while let Some(k) = args.next() {
        flags.insert(k.trim_start_matches("--").to_string(), args.next().context("flag without a value")?);
    }
    let get = |k: &str, d: &str| flags.get(k).cloned().unwrap_or_else(|| d.to_string());
    let dir = flags.get("checkpoint").context("--checkpoint")?.clone();
    let used = mem::used_gb()?;
    if used + 10.0 >= get("max-used-gb", "100").parse::<f64>()? {
        bail!("memory: {used:.1} GB used; not loading the weights");
    }
    match mode.as_str() {
        "burst" => {
            let rows = requests(&flags.get("rows").context("--rows")?.clone(), get("n", "30").parse()?)?;
            let mut snaps = vec![snapshot("start")?];
            let abi = Abi::load(&dir)?;
            snaps.push(snapshot("after load")?);
            let mut lines = String::new();
            for (id, req) in &rows {
                let t = Instant::now();
                let out = abi.decide(req)?;
                let ms = t.elapsed().as_secs_f64() * 1e3;
                lines += &json!({"id": id, "probabilities": out["answers"]["q"]["probabilities"], "ms": ms,
                                 "tokens": out["usage"]["input_tokens"]}).to_string();
                lines.push('\n');
            }
            snaps.push(snapshot("after burst")?);
            std::fs::write(get("out", "/dev/null"), lines)?;
            println!("{}", json!({"policy": std::env::var("D1E_CACHE_LIMIT_MB").unwrap_or_else(|_| "default".into()),
                                   "cap_bytes": cache::limit()?, "rows": rows.len(), "snapshots": snaps}));
        }
        "latency" => {
            // capped: the default policy (cap, cleared after each request); uncapped: MLX's own (cap = its memory
            // limit, never cleared). The request path is the ffi's either way; only the policy flips per row.
            let rows = requests(&flags.get("rows").context("--rows")?.clone(), get("n", "20").parse()?)?;
            let warm: usize = get("warmup", "3").parse()?;
            let abi = Abi::load(&dir)?;
            let uncapped = mlx_rs::memory::memory_limit().map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut ms: HashMap<&str, Vec<f64>> = HashMap::new();
            for (i, (_, req)) in rows.iter().enumerate() {
                let order: [&str; 2] = if i % 2 == 0 { ["capped", "uncapped"] } else { ["uncapped", "capped"] };
                for arm in order {
                    // each arm starts from its own steady state: capped from an empty cache (as after its last
                    // request), uncapped from a cache this row's untimed run filled
                    if arm == "capped" {
                        std::env::remove_var("D1E_CACHE_LIMIT_MB");
                        mlx_rs::memory::set_cache_limit(cache::DEFAULT_LIMIT).ok();
                        mlx_rs::memory::clear_cache().ok();
                    } else {
                        std::env::set_var("D1E_CACHE_LIMIT_MB", "off");
                        mlx_rs::memory::set_cache_limit(uncapped).ok();
                        abi.decide(req)?;
                    }
                    let t = Instant::now();
                    abi.decide(req)?;
                    if i >= warm {
                        ms.entry(arm).or_default().push(t.elapsed().as_secs_f64() * 1e3);
                    }
                }
            }
            let median = |v: &mut Vec<f64>| {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            };
            let mut out = serde_json::Map::new();
            for (arm, mut v) in ms {
                out.insert(arm.into(), json!({"n": v.len(), "median_ms": median(&mut v), "max_ms": v.last()}));
            }
            println!("{}", Value::Object(out));
        }
        other => bail!("unknown mode {other}"),
    }
    Ok(())
}
