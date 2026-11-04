#![cfg(target_os = "macos")]
//! Decider V6 loads and answers through the app's path (`precision::resolve` then `LocalDecider::load_pick`, the
//! d1v5 MLX engine): a load check on a few sentences, not a benchmark or a quality score (those are runs/v6/report.json).
//!
//!   cargo nextest run -p decide --run-ignored only v6_loads
use anyhow::Result;
use decide::precision::{self, DeciderModel, Pick};
use decide::*;
use std::path::PathBuf;

fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_DECIDE_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/decide"))
}

#[test]
#[ignore = "loads Decider V6 (2B, the app's 4-bit by default; HEIMDALL_V6_PICK=16bit|8bit|4bit) on the MLX engine"]
fn v6_loads_and_answers_through_the_app_path() -> Result<()> {
    let m = models();
    let precision = std::env::var("HEIMDALL_V6_PICK").ok().and_then(|k| precision::Precision::from_key(&k)).unwrap_or_default();
    let chosen = Pick::new(DeciderModel::V6, precision);
    let resolved = precision::resolve(chosen, |p| precision::present(&m, p));
    assert_eq!(resolved.used, chosen, "V6 is on this Mac: {:?}", resolved.reason());
    let archive = tempfile::tempdir()?;
    let started = std::time::Instant::now();
    let mut d = LocalDecider::load_pick(&m, archive.path(), resolved.used)?;
    println!("decider: {} loaded in {:.1} s", resolved.used.describe(), started.elapsed().as_secs_f64());
    assert!(d.command_runtime().starts_with("d1v5-engine@"), "{}", d.command_runtime());
    assert!(d.suggests(), "V6 asks V5's questions (suggest too)");
    let mut c = send::Conversation::default();
    for text in ["Okay.", "Heimdall, what's the weather in Poznań tomorrow?", "So the defaults are fine, I think.",
                 "Heimdall, otwórz proszę kalendarz.", "Yeah, I'll call you back in five minutes."] {
        c.append("User", text, true);
        let snapshot = c.snapshot();
        let (_, p) = d.send_decision(&snapshot)?;
        let total: f32 = p.probabilities.values().sum();
        assert!((total - 1.0).abs() < 1e-3, "probabilities sum to {total}");
        println!("{text:?} -> {} {:.2} (ack {:?})", p.choice.key(), p.probabilities.get(p.choice.key()).copied().unwrap_or_default(), p.ack);
        if p.choice == send::Choice::Send { c.sent(snapshot.through); }
    }
    Ok(())
}
