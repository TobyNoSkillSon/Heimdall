//! Compare the shipped Metal path to pinned CPU NeMo on the same real samples.
use std::{path::PathBuf, process::Command};
#[test]
fn checkpoint_frontend_and_first_chunk_match_nemo() {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    if !models.join("ami-480.wav").is_file() {
        eprintln!("skipped: pinned public AMI crop missing");
        return;
    }
    let dir = std::env::temp_dir().join(format!("heimdall-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_heimdall-speakers-file"))
        .args([
            models.as_os_str(),
            models.join("ami-480.wav").as_os_str(),
            dir.join("receipt.json").as_os_str(),
            std::ffi::OsStr::new("1"),
            std::ffi::OsStr::new("gpu"),
            dir.join("probs.f32").as_os_str(),
        ])
        .env("HEIMDALL_DIAR_FEATURE_DUMP", dir.join("features.f32"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/reference-first-chunk.json")).unwrap();
    for (name, file, max_bound, rms_bound) in [
        ("features", "features.f32", 0.0001_f32, 0.000005_f32),
        ("probabilities", "probs.f32", 0.001, 0.0002),
    ] {
        let bytes = std::fs::read(dir.join(file)).unwrap();
        let actual: Vec<_> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        let golden = expected[name].as_array().unwrap();
        assert!(actual.len() >= golden.len());
        let errors: Vec<_> = golden
            .iter()
            .zip(actual)
            .map(|(e, a)| a - e.as_f64().unwrap() as f32)
            .collect();
        let max = errors.iter().map(|e| e.abs()).fold(0_f32, f32::max);
        let rms = (errors.iter().map(|e| e * e).sum::<f32>() / errors.len() as f32).sqrt();
        assert!(
            max < max_bound && rms < rms_bound,
            "{name}: max={max}, rms={rms}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}
