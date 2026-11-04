#![cfg(target_os = "macos")]
use listen::{AudioChunk, ListenEngine, ListenEvent, Recognizer};
use std::path::Path;
#[test]
#[ignore = "needs the .wav audio fixtures that are not in the public repository"]
fn pinned_ultra_whole_clip_goldens() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gold: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("tests/fixtures/golden.json")).unwrap())
            .unwrap();
    let mut asr = listen::asr::Parakeet::load(&root.join("../../models/parakeet-ultra"))
        .expect("fetch pinned models with listen/scripts/fetch_models.py");
    for row in gold.as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let audio = listen::file::read_wav(&root.join(format!("tests/fixtures/{id}.wav"))).unwrap();
        let transcript = asr.transcribe(&audio, 0).unwrap();
        assert_eq!(transcript.text, row["text"].as_str().unwrap(), "{id}");
        assert!(!transcript.words.is_empty());
        for w in &transcript.words {
            assert!(w.start_ns <= w.end_ns && w.end_ns <= audio.len() as u64 * 62500);
        }
        for pair in transcript.words.windows(2) {
            assert!(pair[0].start_ns <= pair[1].start_ns);
        }
    }
}
#[test]
#[ignore = "needs the .wav audio fixtures that are not in the public repository"]
fn engine_stream_goldens_and_revisable_confirmation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let gold: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("tests/fixtures/engine-golden.json")).unwrap(),
    )
    .unwrap();
    let asr = listen::asr::Parakeet::load(&root.join("../../models/parakeet-ultra")).unwrap();
    let mut engine = ListenEngine::load(&root.join("../../models"), asr).unwrap();
    for row in gold.as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let audio = listen::file::read_wav(&root.join(format!("tests/fixtures/{id}.wav"))).unwrap();
        let mut events = vec![];
        for (i, chunk) in audio.chunks(320).enumerate() {
            events.extend(
                engine
                    .push(AudioChunk {
                        source: "mic",
                        generation: 0,
                        host_time_ns: i as u64 * 20_000_000,
                        samples: chunk,
                    })
                    .unwrap(),
            );
        }
        events.extend(engine.flush_source("mic").unwrap());
        let mut segment_text = vec![];
        let mut finals = vec![];
        for event in events {
            match event {
                ListenEvent::PartialTranscript {
                    words,
                    stable_words,
                    ..
                } => {
                    assert!(stable_words <= words.len(), "{id}");
                    assert!(words.len() <= 2048, "partial exceeds decoder bound: {id}");
                }
                ListenEvent::FinalUtterance {
                    text,
                    words,
                    start_ns,
                    end_ns,
                    end_of_turn_probability,
                    ..
                } => {
                    assert!(start_ns < end_ns);
                    assert!((0.0..=1.0).contains(&end_of_turn_probability));
                    for w in words {
                        assert!(start_ns <= w.start_ns && w.start_ns <= w.end_ns);
                        assert!(w.end_ns <= end_ns && end_ns <= audio.len() as u64 * 62500);
                    }
                    segment_text.push(text);
                    finals.push(segment_text.join(" "));
                    segment_text.clear();
                }
                ListenEvent::TranscriptSegment { text, words, .. } => {
                    assert!(words.len() <= 2048);
                    segment_text.push(text);
                }
                _ => {}
            }
        }
        let lexical = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_alphanumeric() || c.is_whitespace() {
                        c
                    } else {
                        ' '
                    }
                })
                .collect::<String>()
                .to_lowercase()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            finals.iter().map(|s| lexical(s)).collect::<Vec<_>>(),
            row["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| lexical(s.as_str().unwrap()))
                .collect::<Vec<_>>(),
            "{id}"
        );
    }
}
