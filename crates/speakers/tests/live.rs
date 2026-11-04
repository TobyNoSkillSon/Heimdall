use speakers::{live::LiveStrategy, online::OnlineConfig, pipeline::Diarization};
use std::path::PathBuf;
#[test]
fn selectable_streams_preserve_geometry_tail_and_chunk_partition() {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    let wav = models.join("ami-480.wav");
    if !wav.is_file() {
        eprintln!("skipped: ignored corpus missing");
        return;
    }
    let mut r = hound::WavReader::open(wav).unwrap();
    let pcm: Vec<_> = r
        .samples::<i16>()
        .take(8 * 16000 + 117)
        .map(|x| x.unwrap() as f32 / 32768.)
        .collect();
    for strategy in [
        LiveStrategy::Nemotron,
        LiveStrategy::Sortformer,
        LiveStrategy::Embedding,
        LiveStrategy::Hybrid,
    ] {
        if strategy == LiveStrategy::Sortformer && !models.join("Sortformer_v2.1.mlmodelc").is_dir()
        {
            eprintln!("skipped: Sortformer missing");
            continue;
        }
        let config = OnlineConfig {
            window_seconds: 3.,
            cosine_threshold: 0.43815252,
            ..Default::default()
        };
        let mut a = strategy.load(&models, 0, config.clone()).unwrap();
        assert_eq!(a.sample_rate(), 16000);
        assert_eq!(
            a.speakers(),
            if strategy == LiveStrategy::Sortformer {
                4
            } else {
                8
            }
        );
        assert!(
            (a.seconds_per_frame()
                - if strategy == LiveStrategy::Sortformer {
                    0.08
                } else {
                    0.01
                })
            .abs()
                < 1e-8
        );
        a.push_f32(&pcm).unwrap();
        a.finish().unwrap();
        let expected = a.segments().unwrap();
        let frames = a.frame_count();
        a.finish().unwrap();
        assert_eq!(expected, a.segments().unwrap());
        assert!(a.push_f32(&[0.]).is_err());
        drop(a);
        let mut b = strategy.load(&models, 0, config).unwrap();
        for c in pcm.chunks(317) {
            b.push_f32(c).unwrap();
        }
        b.finish().unwrap();
        assert_eq!(b.frame_count(), frames);
        let got = b.segments().unwrap();
        assert_eq!(expected.len(), got.len(), "{strategy:?}");
        for (a, b) in expected.iter().zip(got) {
            assert_eq!(a.speaker, b.speaker);
            assert!(
                (a.start_time - b.start_time).abs() < 1e-6
                    && (a.end_time - b.end_time).abs() < 1e-6,
                "{strategy:?}: {a:?} {b:?}"
            );
        }
        assert!(expected
            .iter()
            .all(|s| s.end_time <= pcm.len() as f64 / 16000. + 1e-6));
    }
}
#[test]
fn selection_is_explicit() {
    assert_eq!(LiveStrategy::default(), LiveStrategy::Nemotron);
    assert_eq!(LiveStrategy::RUNNER_UP, LiveStrategy::Sortformer);
    assert!(LiveStrategy::Sortformer.validate_participants(6).is_err());
    assert!(LiveStrategy::Nemotron.validate_participants(6).is_ok());
    assert!(LiveStrategy::parse("community-1").is_err());
    assert_eq!(
        LiveStrategy::parse("nemotron").unwrap(),
        LiveStrategy::Nemotron
    );
}
