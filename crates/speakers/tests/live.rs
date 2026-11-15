use speakers::{diar::Diarizer, pipeline::Diarization};
use std::path::PathBuf;
#[test]
fn nemotron_stream_preserves_geometry_tail_and_chunk_partition() {
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
    let load = || Diarizer::load(&models.join("Nemotron-3-Diarization.q8_0.gguf"), 0).unwrap().stream(16000).unwrap();
    {
        let mut a = load();
        assert_eq!(a.sample_rate(), 16000);
        assert_eq!(a.speakers(), 8);
        assert!((a.seconds_per_frame() - 0.01).abs() < 1e-8);
        a.push_f32(&pcm).unwrap();
        a.finish().unwrap();
        let expected = a.segments().unwrap();
        let frames = a.frame_count();
        a.finish().unwrap();
        assert_eq!(expected, a.segments().unwrap());
        assert!(a.push_f32(&[0.]).is_err());
        drop(a);
        let mut b = load();
        for c in pcm.chunks(317) {
            b.push_f32(c).unwrap();
        }
        b.finish().unwrap();
        assert_eq!(b.frame_count(), frames);
        let got = b.segments().unwrap();
        assert_eq!(expected.len(), got.len());
        for (a, b) in expected.iter().zip(got) {
            assert_eq!(a.speaker, b.speaker);
            assert!(
                (a.start_time - b.start_time).abs() < 1e-6
                    && (a.end_time - b.end_time).abs() < 1e-6,
                "{a:?} {b:?}"
            );
        }
        assert!(expected
            .iter()
            .all(|s| s.end_time <= pcm.len() as f64 / 16000. + 1e-6));
    }
}
