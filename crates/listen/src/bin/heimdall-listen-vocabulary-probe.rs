//! Local-only, resident ABBA/BAAB vocabulary accuracy and decode-cost probe.
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    use listen::{asr::Parakeet, vocabulary::Vocabulary, Recognizer};
    use std::{path::PathBuf, time::Instant};
    let mut args = std::env::args().skip(1);
    let manifest = PathBuf::from(args.next().context("missing local manifest")?);
    let rounds: usize = args.next().unwrap_or_else(|| "1".into()).parse()?;
    anyhow::ensure!((1..=16).contains(&rounds), "rounds must be 1..=16");
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(manifest)?)?;
    anyhow::ensure!(!rows.is_empty(), "manifest is empty");
    let audio = rows
        .iter()
        .map(|r| {
            listen::file::read_wav(std::path::Path::new(
                r["path"].as_str().context("missing WAV")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/parakeet-ultra");
    let mut asr = Parakeet::load(&models)?;
    let before = Vocabulary {
        words: vec![],
        ..Vocabulary::default()
    };
    let after = Vocabulary::default();
    // One warm decode per arm, excluded. Configuration and PCM loading are not timed.
    for config in [&before, &after] {
        asr.set_vocabulary(config.clone())?;
        asr.transcribe(&audio[0], 0)?;
    }
    for round in 0..rounds {
        for (row, pcm) in rows.iter().zip(&audio) {
            let order = if round % 2 == 0 {
                [false, true, true, false]
            } else {
                [true, false, false, true]
            };
            for enabled in order {
                asr.set_vocabulary(if enabled {
                    after.clone()
                } else {
                    before.clone()
                })?;
                let start = Instant::now();
                let transcript = asr.transcribe(pcm, 0)?;
                let seconds = start.elapsed().as_secs_f64();
                anyhow::ensure!(
                    asr.backend() == "metal",
                    "probe fell back from the default Metal path"
                );
                println!(
                    "{}",
                    serde_json::json!({"id":row["id"],"enabled":enabled,"round":round,"audio_seconds":pcm.len() as f64/16000.,"seconds":seconds,"transcript":transcript})
                );
            }
        }
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() -> Result<()> {
    anyhow::bail!("Parakeet requires macOS")
}
