//! Read-only memory reading for the check binary: Activity Monitor's "Memory Used" from vm_stat, as
//! lab/decider-v5/mem.py `used_gb` computes it (anonymous less purgeable, plus wired and compressed pages).

use anyhow::{Context, Result};
use std::process::Command;

pub fn used_gb() -> Result<f64> {
    let out = String::from_utf8(Command::new("vm_stat").output()?.stdout)?;
    let page: f64 = out
        .split("page size of ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .context("vm_stat page size")?
        .parse()?;
    let get = |name: &str| -> f64 {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}:")))
            .and_then(|v| v.trim().trim_end_matches('.').parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let app = get("Anonymous pages") - get("Pages purgeable");
    Ok((app + get("Pages wired down") + get("Pages occupied by compressor")) * page / (1u64 << 30) as f64)
}
