use speakers::{
    diar::Diarizer,
    embedding::{ReDimNet, VoiceEncoder},
    identity::EnrollmentStore,
    naming::{NamingFlow, SpeakerEvent},
    pipeline::SpeakerPipeline,
};
use std::path::PathBuf;
#[test]
fn native_models_stream_finish_identify_and_name_offline() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    if !root.join("Nemotron-3-Diarization.q8_0.gguf").exists() {
        eprintln!("offline native test needs pinned speakers models; unit/ABI tests still ran");
        return;
    }
    let diar = Diarizer::load(&root.join("Nemotron-3-Diarization.q8_0.gguf"), 0).unwrap();
    assert_eq!(diar.speakers, 8);
    assert!((diar.seconds_per_frame - 0.01).abs() < 1e-6);
    let mut encoder = ReDimNet::load(&root.join("ReDimNet2B6.onnx")).unwrap();
    let mut wav = hound::WavReader::open(root.join("ami-24000.wav")).unwrap();
    let audio: Vec<f32> = wav
        .samples::<i16>()
        .take(120 * 16000)
        .map(|s| s.unwrap() as f32 / 32768.)
        .collect();
    let mut stream = diar.stream(16000).unwrap();
    assert_eq!(stream.frame_count(), 0);
    stream.push_f32(&audio[..16000]).unwrap();
    assert!(stream.frame_count() > 0);
    assert!(stream.push_f32(&[f32::NAN]).is_err());
    stream.push_f32(&audio[16000..]).unwrap();
    stream.finish().unwrap();
    stream.finish().unwrap();
    assert!(stream.push_f32(&[0.]).is_err());
    let frames = stream.frame_probabilities().unwrap();
    assert!(frames.frame_count >= 2990);
    assert_eq!(frames.values.len(), (frames.frame_count-frames.first_frame) as usize * 8);
    assert!(frames.first_frame > 0, "two minutes must retire a stable prefix");
    assert!(frames.frame_count-frames.first_frame <= 6000);
    assert!(!stream.take_finalized_segments().unwrap().is_empty());
    assert!(!stream.segments().unwrap().is_empty());
    drop(stream);
    let windows: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("identity-windows.json")).unwrap())
            .unwrap();
    let parts = windows["FEE013"][4]["parts"].as_array().unwrap();
    // Read only this reference-clean held-out crop, not a mixed six-second diagnostic.
    let mut wav = hound::WavReader::open(root.join("ami-24000.wav")).unwrap();
    let mut clean = vec![];
    for p in parts {
        let a = p[0].as_u64().unwrap() as u32;
        let b = p[1].as_u64().unwrap() as u32;
        wav.seek(a).unwrap();
        clean.extend(
            wav.samples::<i16>()
                .take((b - a) as usize)
                .map(|s| s.unwrap() as f32 / 32768.),
        );
    }
    let embedded = encoder.embed(&clean).unwrap();
    assert_eq!(embedded.values().len(), 192);
    let norm = embedded.values().iter().map(|x| x * x).sum::<f32>();
    assert!((norm - 1.).abs() < 0.001);
    let store = EnrollmentStore::load(&root.join("evaluation-enrollments.json")).unwrap();
    let identity = store.identify(&embedded);
    println!("held-out native identity: {identity:?}");
    let naming = NamingFlow::new(EnrollmentStore::default());
    let mut pipeline = SpeakerPipeline::new(
        diar.stream(16000).unwrap(),
        encoder,
        naming,
        "Discord".into(),
        1,
        100.,
    )
    .unwrap();
    let mut events = pipeline.push(&audio).unwrap();
    events.extend(pipeline.finish().unwrap());
    assert!(events.iter().any(|event|matches!(event,SpeakerEvent::VoiceCoded{new:true,..})));
    assert!(!pipeline.segments().unwrap().is_empty());
}
