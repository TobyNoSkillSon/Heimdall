//! Short, explicit reviewer checks; require the ignored pinned local models/corpus.
use speakers::{
    diar::Diarizer,
    embedding::{ReDimNet, VoiceEncoder},
    identity::{EnrollmentStore, Identity},
};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers")
}
fn pcm(path: PathBuf) -> Vec<f32> {
    hound::WavReader::open(path)
        .unwrap()
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.)
        .collect()
}

#[test]
#[ignore = "explicit short native review; pinned local assets required"]
fn fresh_streams_and_feed_partition_preserve_predictions() {
    let root = root();
    let audio = pcm(root.join("ami-480.wav"));
    let audio = &audio[..5 * 16000];
    let model = Diarizer::load(&root.join("Nemotron-3-Diarization.q8_0.gguf"), 0).unwrap();
    let mut first = model.stream(16000).unwrap();
    for chunk in audio.chunks(320) {
        first.push_f32(chunk).unwrap();
    }
    first.finish().unwrap();
    let expected = first.frame_probabilities().unwrap();
    assert!((500..=501).contains(&expected.frame_count));
    // Retain one completed stream, then interleave two new streams sharing the model.
    let mut second = model.stream(16000).unwrap();
    let mut third = model.stream(16000).unwrap();
    second.push_f32(audio).unwrap();
    for chunk in audio.chunks(997) {
        third.push_f32(chunk).unwrap();
    }
    second.finish().unwrap();
    third.finish().unwrap();
    for actual in [
        second.frame_probabilities().unwrap(),
        third.frame_probabilities().unwrap(),
    ] {
        assert_eq!(actual.frame_count, expected.frame_count);
        let difference = actual
            .values
            .iter()
            .zip(&expected.values)
            .map(|(a, b)| (a - b).abs())
            .fold(0_f32, f32::max);
        println!("fresh stream / partition max probability difference: {difference}");
        assert!(difference < 1e-5);
    }
}

#[test]
#[ignore = "explicit 55-window native identity review; pinned local assets required"]
fn raw_identity_replays_all_held_out_windows() {
    let root = root();
    let audio = pcm(root.join("ami-480.wav"));
    let indexed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("identity-windows.json")).unwrap())
            .unwrap();
    let store = EnrollmentStore::load(&root.join("evaluation-enrollments.json")).unwrap();
    let mut encoder = ReDimNet::load(&root.join("ReDimNet2B6.onnx")).unwrap();
    let (mut total, mut correct, mut rejected) = (0, 0, 0);
    for (name, windows) in indexed.as_object().unwrap() {
        let mut unknown_store = store.clone();
        unknown_store.names.remove(name);
        let (mut person_total, mut person_correct) = (0, 0);
        for window in &windows.as_array().unwrap()[4..] {
            let mut clean = vec![];
            for part in window["parts"].as_array().unwrap() {
                let a = part[0].as_u64().unwrap() as usize;
                let b = part[1].as_u64().unwrap() as usize;
                clean.extend_from_slice(&audio[a..b]);
            }
            assert_eq!(clean.len(), 3 * 16000);
            let embedding = encoder.embed(&clean).unwrap();
            let right = matches!(store.identify(&embedding), Identity::Known { name: got, .. } if &got == name);
            correct += usize::from(right);
            person_correct += usize::from(right);
            rejected += usize::from(matches!(
                unknown_store.identify(&embedding),
                Identity::Unknown { .. }
            ));
            total += 1;
            person_total += 1;
        }
        println!("{name}: {person_correct}/{person_total}");
    }
    println!("raw identity: {correct}/{total}; unknown rejection: {rejected}/{total}");
    assert_eq!((correct, total, rejected), (40, 55, 54));
}
