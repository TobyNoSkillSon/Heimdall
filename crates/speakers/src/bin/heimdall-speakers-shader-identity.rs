//! Agent-only resident encoder parity/identity probe on the existing held-out
//! cross-session enrollment corpus; no downloads or real profile writes.
use anyhow::{Result, ensure};
use speakers::{
    embedding::{Embedding, ReDimNet, VoiceEncoder},
    identity::{EnrollmentStore, Identity},
};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};
fn window(row: &serde_json::Value) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(row["wav"].as_str().unwrap())?;
    let mut pcm = Vec::new();
    ensure!(
        r.spec().sample_rate == 16000 && r.spec().channels == 1 && r.spec().bits_per_sample == 16,
        "unexpected identity PCM format"
    );
    for pair in row["ranges"].as_array().unwrap() {
        let a = pair[0].as_u64().unwrap() as u32;
        let b = pair[1].as_u64().unwrap() as u32;
        r.seek(a)?;
        pcm.extend(
            r.samples::<i16>()
                .take((b - a) as usize)
                .map(|v| v.map(|s| s as f32 / 32768.))
                .collect::<Result<Vec<_>, _>>()?,
        );
    }
    ensure!(pcm.len() == 96000, "invalid identity window");
    Ok(pcm)
}
fn cpu() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64
        + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
}
fn main() -> Result<()> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    ensure!(a.len() >= 2, "models-dir output-json [repeat]");
    let models = PathBuf::from(&a[0]);
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(models.join("identity-v2/embeddings.json"))?)?;
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| (r["split"] == "test" || r["split"] == "external") && r["length"] == 6)
        .collect();
    let mut encoder = ReDimNet::load(&models.join("ReDimNet2B6.onnx"))?;
    if let Some(repeats) = a.get(2) {
        let pcm = window(rows[0])?;
        encoder.embed(&pcm)?;
        let repeats: usize = repeats.parse()?;
        ensure!(repeats > 0, "repeat count must be positive");
        #[cfg(target_os = "macos")]
        let gpu = speakers::metal::gpu_seconds();
        let measurement_start_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        let before = cpu();
        let start = Instant::now();
        for _ in 0..repeats {
            std::hint::black_box(encoder.embed(&pcm)?);
        }
        let wall = start.elapsed().as_secs_f64();
        let measurement_end_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs_f64();
        #[cfg(target_os = "macos")]
        let gpu = speakers::metal::gpu_seconds() - gpu;
        #[cfg(not(target_os = "macos"))]
        let gpu = 0.;
        std::fs::write(
            &a[1],
            serde_json::to_vec_pretty(
                &serde_json::json!({"measurement_start_unix":measurement_start_unix,"measurement_end_unix":measurement_end_unix,"audio_seconds":(repeats*6),"processing_seconds":wall,"rtf":wall/(repeats*6) as f64,"cpu_seconds":cpu()-before,"gpu_seconds":gpu,"embeddings":repeats,"warmup_embeddings":1}),
            )?,
        )?;
        return Ok(());
    }
    let key = |identity: Identity| match identity {
        Identity::Known { name, .. } => name,
        Identity::Unknown { reason, .. } => format!("Unknown:{reason:?}"),
    };
    let mut cosines = Vec::new();
    let mut summary = Vec::new();
    let mut all_counts = BTreeMap::new();
    for (corpus, split) in [("AMI", "test"), ("VoxConverse", "external")] {
        let selected: Vec<_> = rows.iter().filter(|r| r["split"] == split).collect();
        let mut store = EnrollmentStore::default();
        let mut old_store = EnrollmentStore::default();
        for row in selected.iter().filter(|r| {
            r["codec"] == 0
                && r["index"].as_u64().unwrap() < 2
                && (r["role"] == "enroll" || r["role"] == "both")
        }) {
            let actual = encoder.embed(&window(row)?)?;
            let expected = Embedding::new(serde_json::from_value(row["embedding"].clone())?)?;
            cosines.push(actual.cosine(&expected));
            let name = row["person"].as_str().unwrap();
            store.enroll(name, actual)?;
            old_store.enroll(name, expected)?;
        }
        let mut counts: BTreeMap<u64, BTreeMap<String, [usize; 5]>> = BTreeMap::new();
        for row in selected.iter().filter(|r| {
            r["role"] == "query" || (r["role"] == "both" && r["index"].as_u64().unwrap() >= 2)
        }) {
            let actual = encoder.embed(&window(row)?)?;
            let expected = Embedding::new(serde_json::from_value(row["embedding"].clone())?)?;
            cosines.push(actual.cosine(&expected));
            let name = row["person"].as_str().unwrap();
            let v = counts
                .entry(row["codec"].as_u64().unwrap())
                .or_default()
                .entry(name.into())
                .or_default();
            v[0] += 1;
            let candidate = key(store.identify(&actual));
            let existing = key(old_store.identify(&actual));
            let baseline = key(old_store.identify(&expected));
            v[1] += usize::from(candidate == name);
            v[2] += usize::from(existing == name);
            v[3] += usize::from(baseline == name);
            v[4] += usize::from(candidate == baseline && existing == baseline);
        }
        for (codec, people) in &counts {
            let balanced = |column: usize| {
                100. * people
                    .values()
                    .map(|v| v[column] as f64 / v[0] as f64)
                    .sum::<f64>()
                    / people.len() as f64
            };
            summary.push(serde_json::json!({"corpus":corpus,"codec_kbps":codec,"people":people.len(),"queries":people.values().map(|v|v[0]).sum::<usize>(),"balanced_accuracy_percent":balanced(1),"existing_bank_balanced_accuracy_percent":balanced(2),"baseline_balanced_accuracy_percent":balanced(3),"both_bank_outcome_agreement_percent":balanced(4)}));
        }
        all_counts.insert(corpus, counts);
    }
    std::fs::write(
        &a[1],
        serde_json::to_vec_pretty(
            &serde_json::json!({"embedding_count":cosines.len(),"minimum_cosine":cosines.iter().copied().fold(1_f32,f32::min),"identity":summary,"counts":all_counts}),
        )?,
    )?;
    Ok(())
}
