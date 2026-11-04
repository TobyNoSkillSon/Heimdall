use listen::onnx::{Silero, SmartTurn, VadState};
#[test]
#[ignore = "needs the .wav audio fixtures that are not in the public repository"]
fn recurrent_states_are_independent_and_turn_uses_whisper() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models");
    let path = root.join("silero/silero_vad.onnx");
    let mut vad =
        Silero::load(&path).expect("fetch pinned models with listen/scripts/fetch_models.py");
    let audio = listen::file::read_wav(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/en-1.wav"),
    )
    .unwrap();
    let mut a = VadState::default();
    let mut b = VadState::default();
    for samples in audio.as_chunks::<512>().0.iter().take(40) {
        let pa = vad.probability(samples, &mut a).unwrap();
        let pb = vad.probability(samples, &mut b).unwrap();
        assert!((pa - pb).abs() < 1e-7);
    }
    let mut turn = SmartTurn::load(&root.join("smart-turn/smart-turn-v3.2-cpu.onnx")).unwrap();
    let p = turn.probability(&audio).unwrap();
    assert!((0.0..=1.0).contains(&p));
}

#[test]
#[ignore = "needs the .wav audio fixtures that are not in the public repository"]
fn batched_vad_preserves_each_streams_recurrent_state() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut vad = Silero::load(&root.join("../../models/silero/silero_vad.onnx")).unwrap();
    let audio = listen::file::read_wav(&root.join("tests/fixtures/en-2.wav")).unwrap();
    let mut batched = vec![VadState::default(); 6];
    let mut serial = batched.clone();
    for frame in 0..40 {
        let blocks: Vec<[f32; 512]> = (0..6)
            .map(|i| {
                let start = (frame * 512 + i * 1733) % (audio.len() - 512);
                audio[start..start + 512].try_into().unwrap()
            })
            .collect();
        let probabilities = vad.probabilities(&blocks, &mut batched).unwrap();
        for i in 0..6 {
            let p = vad.probability(&blocks[i], &mut serial[i]).unwrap();
            assert!(
                (p - probabilities[i]).abs() < 1e-6,
                "stream {i}, frame {frame}"
            );
        }
    }
    assert!(vad.probabilities(&[], &mut []).is_err());
    assert!(vad.probabilities(&[[0.; 512]], &mut []).is_err());
}
