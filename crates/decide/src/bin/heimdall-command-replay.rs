//! Resident, permission-free command replay. JSONL inputs/receipts remain private.
use anyhow::{Context, Result};
use decide::*;
use serde::Deserialize;
use serde_json::json;
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    time::Instant,
};
#[derive(Deserialize)]
struct Request {
    candidate: CommandCandidate,
    input: DecisionInput,
    /// Early (audio at detection, no transcript veto) or final (default).
    #[serde(default)]
    stage: CommandStage,
}
fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let models = root.join("models/decide");
    let archive = root.join("lab/eval/data/chain-quality/probe-archive");
    let mut decider = LocalDecider::load(&models, archive)?;
    println!(
        "{}",
        json!({"ready": true, "runtime": decider.command_runtime()})
    );
    io::stdout().flush()?;
    let started = Instant::now();
    for line in io::stdin().lock().lines() {
        anyhow::ensure!(started.elapsed().as_secs() < 540, "replay budget exhausted");
        let request: Request = serde_json::from_str(&line?).context("command replay request")?;
        let start = Instant::now();
        let result = json!({"id": request.input.current.id, "decision": decider.command_decision_at(request.candidate, &request.input, request.stage)});
        println!(
            "{}",
            json!({"result":result,"ms":start.elapsed().as_secs_f64()*1000.0})
        );
        io::stdout().flush()?;
    }
    Ok(())
}
