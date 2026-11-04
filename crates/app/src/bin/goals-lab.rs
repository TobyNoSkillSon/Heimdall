//! Lab only, never in the bundle: Background Tasks ▸ Goals' shadow (docs/goals.md) on a recorded day, with the app's own
//! code (the files below are the app's, included as they are).
//!
//!   goals-lab replay --session <pi jsonl> --from <iso> --to <iso> --seed default|empty|<goals.json> --runner <cmd> \
//!       --out <dir> [--effort low] [--compact-tokens N] [--seed-settled <settled.json>]
//!   goals-lab stream --session <pi jsonl> --from <iso> --to <iso>
//!   goals-lab prompt
//!
//! replay: the window in simulated time (triggers, turns, checks, apply into `<dir>/islands-root`), each call through
//! the runner (JSON on stdin, `{"text", "usage", "ms"}` on stdout; `lab/goals/mock-runner.py` answers no ops);
//! writes `<dir>/turns.jsonl` and `<dir>/final.json`. stream: the rendered lines with their times. prompt: the system
//! prompt's size and token estimate.
#![allow(dead_code)]

// Included as they are; `#[path]` inside an inline module would look under src/bin/, so each file is a top-level
// module here and `islands`/`goals` re-export them at the paths the app's code names (`crate::goals::ops`, …).
#[path = "../pi_session.rs"]
mod pi_session;
#[path = "../islands/files.rs"]
mod islands_files;
#[path = "../goals/lab.rs"]
mod goals_lab;
#[path = "../goals/ledger.rs"]
mod goals_ledger;
#[path = "../goals/ops.rs"]
mod goals_ops;
#[path = "../goals/proof.rs"]
mod goals_proof;
#[path = "../goals/segment.rs"]
mod goals_segment;
#[path = "../goals/settled.rs"]
mod goals_settled;
#[path = "../goals/shadow.rs"]
mod goals_shadow;
#[path = "../goals/stream.rs"]
mod goals_stream;
#[path = "../goals/trigger.rs"]
mod goals_trigger;
#[path = "../islands/plan.rs"]
mod islands_plan;
#[path = "../islands/proposals.rs"]
mod islands_proposals;
// plan.rs and proposals.rs name their siblings as `super::files` and `super::tools`; at the top level here that is
// the crate root, so the two are provided here too (`tools` holds the one item plan.rs uses; the real module pulls
// in the whole app).
use islands_files as files;
mod tools {
    pub fn now_s() -> f64 {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        (t * 10.0).round() / 10.0
    }
}
mod islands {
    pub(crate) use super::{islands_files as files, islands_plan as plan, islands_proposals as proposals};
    /// No island host in the lab: a proposal's spoken line (goals/ops.rs) is skipped.
    pub(crate) enum Class { Context }
    pub(crate) struct Host;
    impl Host {
        pub fn deliver(&self, _: Class, _: &str) {}
    }
    pub(crate) fn host() -> Option<std::sync::Arc<Host>> { None }
}
mod goals {
    pub(crate) use super::{goals_lab as lab, goals_ledger as ledger, goals_ops as ops, goals_proof as proof, goals_segment as segment, goals_settled as settled, goals_shadow as shadow, goals_stream as stream, goals_trigger as trigger};
}

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let mut opt = std::collections::HashMap::new();
    while let Some(k) = args.next() {
        let Some(name) = k.strip_prefix("--") else { bail!("unexpected argument {k:?}") };
        opt.insert(name.to_owned(), args.next().with_context(|| format!("--{name} needs a value"))?);
    }
    let get = |k: &str| opt.get(k).cloned().with_context(|| format!("--{k} is required"));
    match cmd.as_str() {
        "replay" => {
            let r = goals::lab::Replay {
                session: PathBuf::from(get("session")?),
                from: goals::lab::parse_time(&get("from")?)?,
                to: goals::lab::parse_time(&get("to")?)?,
                seed: opt.get("seed").cloned().unwrap_or_else(|| "default".into()),
                runner: get("runner")?,
                out: PathBuf::from(get("out")?),
                effort: opt.get("effort").cloned().unwrap_or_else(|| "low".into()),
                compact_tokens: opt.get("compact-tokens").map(|t| t.parse()).transpose()?,
                seed_settled: opt.get("seed-settled").map(PathBuf::from),
            };
            let summary = goals::lab::replay(&r)?;
            println!("{} calls, {} segment(s), usage {}; {}", summary["calls"], summary["segments"], summary["usage"], r.out.join("turns.jsonl").display());
        }
        "stream" => {
            let (from, to) = (goals::lab::parse_time(&get("from")?)?, goals::lab::parse_time(&get("to")?)?);
            for (at, kind, line) in goals::lab::stream_lines(&PathBuf::from(get("session")?), from, to)? {
                use chrono::TimeZone;
                let t = chrono::Local.timestamp_millis_opt(at).single().map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
                println!("{t} {kind:<8} {line}");
            }
        }
        "prompt" => {
            let system = goals::shadow::SYSTEM;
            let schema = goals::ops::schema().to_string();
            println!("goals.md: {} bytes, {} words; ~{} tokens at 4 bytes a token, at most ~{} at 3 (the job's estimate)",
                system.len(), system.split_whitespace().count(), system.len().div_ceil(4), goals::segment::tokens(system.len()));
            println!("schema: {} bytes, ~{} tokens", schema.len(), goals::segment::tokens(schema.len()));
        }
        _ => bail!("usage: goals-lab replay|stream|prompt (see the file's header)"),
    }
    Ok(())
}
