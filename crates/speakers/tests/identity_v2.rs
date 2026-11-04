//! Full explicit replay of the cross-session AMI test through the shipped Rust encoder/store.
use speakers::{
    embedding::{Embedding, ReDimNet, VoiceEncoder},
    identity::{EnrollmentStore, Identity},
};
use std::{collections::BTreeMap, path::PathBuf};
fn window(row: &serde_json::Value) -> Vec<f32> {
    let mut wav = hound::WavReader::open(row["wav"].as_str().unwrap()).unwrap();
    let mut pcm = vec![];
    for pair in row["ranges"].as_array().unwrap() {
        let a = pair[0].as_u64().unwrap() as u32;
        let b = pair[1].as_u64().unwrap() as u32;
        wav.seek(a).unwrap();
        pcm.extend(
            wav.samples::<i16>()
                .take((b - a) as usize)
                .map(|x| x.unwrap() as f32 / 32768.),
        );
    }
    assert_eq!(pcm.len(), 96000);
    pcm
}
#[test]
#[ignore = "explicit 400-embedding replay; requires ignored identity-v2 corpus"]
fn cross_session_accuracy_reproduces_through_rust_default() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("identity-v2/embeddings.json")).unwrap())
            .unwrap();
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| r["split"] == "test" && r["length"] == 6)
        .collect();
    let mut encoder = ReDimNet::load(&root.join("ReDimNet2B6.onnx")).unwrap();
    let mut store = EnrollmentStore::default();
    for row in rows
        .iter()
        .filter(|r| r["role"] == "enroll" && r["codec"] == 0 && r["index"].as_u64().unwrap() < 2)
    {
        store
            .enroll(
                row["person"].as_str().unwrap(),
                encoder.embed(&window(row)).unwrap(),
            )
            .unwrap();
    }
    assert_eq!(store.names.len(), 8);
    assert!(store.names.values().all(|e| e.embeddings.len() == 2));
    let mut results: BTreeMap<u64, BTreeMap<String, (usize, usize)>> = BTreeMap::new();
    let mut max_error = 0_f32;
    for row in rows.iter().filter(|r| r["role"] == "query") {
        let e = encoder.embed(&window(row)).unwrap();
        let expected =
            Embedding::new(serde_json::from_value(row["embedding"].clone()).unwrap()).unwrap();
        for (a, b) in e.values().iter().zip(expected.values()) {
            max_error = max_error.max((a - b).abs());
        }
        let actual = store.identify(&e);
        let person = row["person"].as_str().unwrap();
        let score = results
            .entry(row["codec"].as_u64().unwrap())
            .or_default()
            .entry(person.into())
            .or_default();
        score.1 += 1;
        if matches!(actual,Identity::Known{name,..} if name==person) {
            score.0 += 1;
        }
    }
    assert!(
        max_error < 0.00001,
        "Rust/Python embedding max error {max_error}"
    );
    for (codec, people) in results {
        let correct: usize = people.values().map(|v| v.0).sum();
        let n: usize = people.values().map(|v| v.1).sum();
        let balanced = people
            .values()
            .map(|v| v.0 as f64 / v.1 as f64)
            .sum::<f64>()
            / people.len() as f64;
        println!("codec={codec} kb/s: {correct}/{n}, balanced={:.6}%, 8 speakers; embedding max delta={max_error}",balanced*100.);
        assert_eq!((correct, n), (127, 128));
    }
}
