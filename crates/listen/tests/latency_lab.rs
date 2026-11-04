//! Lab run for the latency map (lab/latency/MAP.md): how long the engine works after it decides a pause ended a turn —
//! Smart Turn, then the final Parakeet decode — on the real pinned models. Each fixture is pushed as fast as the engine
//! takes it, followed by 2.6 s of silence, so the marks measure compute, not waiting. The pause wait itself is the
//! policy's silence (`models/smart-turn/owner-policy.json`), not measured here.
//!
//! `HEIMDALL_LISTEN_LAB=1 cargo test -p listen --release --test latency_lab -- --ignored --nocapture`
#[cfg(target_os = "macos")]
#[test]
#[ignore = "lab run: real models; set HEIMDALL_LISTEN_LAB=1"]
fn final_turn_and_decode_times_after_the_endpoint() -> anyhow::Result<()> {
    use listen::{AudioChunk, ListenEngine, ListenEvent};
    use std::path::Path;
    if std::env::var_os("HEIMDALL_LISTEN_LAB").is_none() {
        return Ok(());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let models = root.join("../../models");
    let asr = listen::asr::Parakeet::load(&models.join("parakeet-ultra"))?;
    let mut engine = ListenEngine::load(&models, asr)?;
    let silence = vec![0f32; 41600];
    let mut ns = 1_000_000_000u64;
    let mut rows = vec![];
    for round in 0..3 {
        for name in ["en-1", "en-2", "pl-1", "pl-2", "continuous"] {
            let audio = listen::file::read_wav(&root.join(format!("tests/fixtures/{name}.wav")))?;
            let source = "mic:latency-lab";
            for chunk in audio.iter().chain(silence.iter()).copied().collect::<Vec<_>>().chunks(320) {
                for event in engine.push(AudioChunk { source, generation: 0, host_time_ns: ns, samples: chunk })? {
                    if let ListenEvent::FinalUtterance { start_ns, end_ns, forced, .. } = event {
                        let t = listen::timing::take(source, end_ns).expect("every final carries its marks");
                        rows.push((round, name, (end_ns - start_ns) as f64 / 1e9, forced,
                            (t.turn_ns - t.endpoint_ns) as f64 / 1e6, (t.decoded_ns - t.turn_ns) as f64 / 1e6));
                    }
                }
                ns += chunk.len() as u64 * 62_500;
            }
        }
    }
    println!("round file utterance_s forced smart_turn_ms final_decode_ms");
    for (round, name, seconds, forced, turn, decode) in &rows {
        println!("{round} {name} {seconds:.2} {forced} {turn:.1} {decode:.1}");
    }
    // Warm rounds only: round 0 includes first-use compilation.
    let warm: Vec<_> = rows.iter().filter(|r| r.0 > 0).collect();
    let median = |mut v: Vec<f64>| { v.sort_by(f64::total_cmp); v[v.len() / 2] };
    println!("warm finals {} median smart_turn_ms {:.1} median final_decode_ms {:.1} worst final_decode_ms {:.1}",
        warm.len(), median(warm.iter().map(|r| r.4).collect()), median(warm.iter().map(|r| r.5).collect()),
        warm.iter().map(|r| r.5).fold(0., f64::max));
    assert!(!warm.is_empty());
    Ok(())
}
