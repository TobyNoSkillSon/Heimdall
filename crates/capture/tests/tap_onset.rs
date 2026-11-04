#![cfg(target_os = "macos")]
// Explicit lab tests only. Regular tests never request capture access or play audio.
use capture::{Archive, Capture, CaptureConfig, CaptureConsent, CaptureEvent, SourceKind};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
fn cpu_seconds() -> f64 {
    let mut r = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    unsafe {
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, r.as_mut_ptr()), 0);
        let r = r.assume_init();
        (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64
            + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
    }
}
fn drain(rx: &capture::CaptureReceiver, seconds: u64) -> f64 {
    let start = Instant::now();
    let cpu = cpu_seconds();
    while start.elapsed() < Duration::from_secs(seconds) {
        if let Ok(CaptureEvent::Gap { detail, .. }) =
            rx.recv_timeout(Duration::from_secs(seconds).saturating_sub(start.elapsed()))
        {
            panic!("capture gap: {detail}");
        }
    }
    (cpu_seconds() - cpu) / start.elapsed().as_secs_f64() * 100.
}
fn wav_mono(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut pos = 12;
    let mut channels = 0;
    while pos + 8 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let data = &bytes[pos + 8..pos + 8 + n];
        match &bytes[pos..pos + 4] {
            b"fmt " => {
                assert_eq!(u16::from_le_bytes(data[..2].try_into().unwrap()), 1);
                channels = u16::from_le_bytes(data[2..4].try_into().unwrap()) as usize;
                assert_eq!(u32::from_le_bytes(data[4..8].try_into().unwrap()), 48000);
                assert_eq!(u16::from_le_bytes(data[14..16].try_into().unwrap()), 16);
            }
            b"data" => {
                return data
                    .chunks_exact(channels * 2)
                    .map(|frame| {
                        frame
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|s| i16::from_le_bytes(*s) as f32 / 32768.)
                            .sum::<f32>()
                            / channels as f32
                    })
                    .collect()
            }
            _ => {}
        }
        pos += 8 + n + n % 2;
    }
    panic!("missing PCM");
}
fn similarity(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut aa, mut bb) = (0., 0., 0.);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (x as f64, y as f64);
        dot += x * y;
        aa += x * x;
        bb += y * y;
    }
    dot / (aa * bb).sqrt().max(1e-30)
}
#[test]
#[ignore = "explicit signed lab only; requires System Audio grant and plays a known WAV"]
fn live_process_onset_receipt() {
    let dir = std::env::var("HEIMDALL_TAP_LAB_DIR").expect("isolated lab directory");
    let wav = std::env::var("HEIMDALL_TAP_LAB_WAV").expect("known 48-kHz PCM16 WAV");
    let reference: Vec<_> = wav_mono(Path::new(&wav)).into_iter().step_by(6).collect();
    let (capture, rx) = Capture::start(
        CaptureConfig {
            archive_dir: Path::new(&dir).join("session-lab"),
            ..CaptureConfig::default()
        },
        CaptureConsent::after_setup(false, true),
    )
    .unwrap();
    drain(&rx, 2);
    println!("idle_cpu_one_core_percent={:.6}", drain(&rx, 8));
    let mut child = Command::new("/usr/bin/afplay").arg(wav).spawn().unwrap();
    let started = Instant::now();
    let mut id = None;
    let mut gaps = vec![];
    let mut discontinuities = 0;
    while started.elapsed() < Duration::from_secs(7) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(CaptureEvent::SourceAdded(info)) if matches!(info.kind,SourceKind::Process {pid,..} if pid==child.id()) => {
                id = Some(info.id)
            }
            Ok(CaptureEvent::Gap { detail, .. }) => gaps.push(detail),
            Ok(CaptureEvent::Audio(c)) if id.as_ref() == Some(&c.source_id) && c.discontinuity => {
                discontinuities += 1
            }
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    println!(
        "post_playback_idle_cpu_one_core_percent={:.6}",
        drain(&rx, 8)
    );
    drop(capture);
    println!(
        "new_stream_discontinuities={discontinuities} gap_events={}",
        gaps.len()
    );
    assert!(gaps.is_empty(), "{gaps:?}");
    assert_eq!(discontinuities, 0, "fresh playback is continuous");
    let id = id.expect("new playback process was never tapped");
    let blocks = Archive::open_read_only(&Path::new(&dir).join("session-lab"))
        .unwrap()
        .read_blocks(&id)
        .unwrap();
    let mono: Vec<_> = blocks
        .iter()
        .flat_map(|b| {
            assert_eq!(b.format.sample_rate, 48000);
            b.samples
                .chunks_exact(b.format.channels as usize)
                .map(|s| s.iter().sum::<f32>() / s.len() as f32)
        })
        .step_by(6)
        .collect();
    assert!(
        mono.len() >= reference.len(),
        "capture shorter than the known phrase: {} < {} samples",
        mono.len(),
        reference.len()
    );
    let prefix = reference.len().min(2400);
    let offset = (0..=mono.len() - reference.len())
        .max_by(|&a, &b| {
            similarity(&mono[a..a + prefix], &reference[..prefix])
                .total_cmp(&similarity(&mono[b..b + prefix], &reference[..prefix]))
        })
        .unwrap();
    let correlation = similarity(&mono[offset..offset + reference.len()], &reference);
    assert!(
        correlation > 0.95,
        "missing or altered phrase: correlation={correlation}"
    );
    println!("onset_loss_ms=0 leading_capture_ms={:.3} full_waveform_correlation={correlation:.6} captured_seconds={:.6} reference_seconds={:.6}", offset as f64/8.,mono.len() as f64/8000.,reference.len() as f64/8000.);
}
#[test]
#[ignore = "explicit lab only; granted system-audio capture vs parked discovery CPU brackets"]
fn dormant_taps_cpu_receipt() {
    let dir = std::env::var("HEIMDALL_TAP_LAB_DIR").expect("isolated lab directory");
    for (n, armed) in [false, true, true, false].into_iter().enumerate() {
        let (cap, rx) = Capture::start(
            CaptureConfig {
                archive_dir: Path::new(&dir).join(n.to_string()).join("session-lab"),
                ..CaptureConfig::default()
            },
            CaptureConsent::after_setup(false, armed),
        )
        .unwrap();
        drain(&rx, 2);
        println!(
            "armed={armed} idle_cpu_one_core_percent={:.6}",
            drain(&rx, 8)
        );
        drop(cap);
    }
}
#[test]
#[ignore = "explicit signed lab only; granted capture of one ten-second digital-silence source"]
fn active_silence_cpu_receipt() {
    let dir = std::env::var("HEIMDALL_TAP_LAB_DIR").expect("isolated lab directory");
    std::fs::create_dir_all(&dir).unwrap();
    let wav = Path::new(&dir).join("silence.wav");
    let bytes = 48000u32 * 10 * 2;
    let mut pcm = Vec::new();
    pcm.extend_from_slice(b"RIFF");
    pcm.extend_from_slice(&(36 + bytes).to_le_bytes());
    pcm.extend_from_slice(b"WAVEfmt ");
    pcm.extend_from_slice(&16u32.to_le_bytes());
    pcm.extend_from_slice(&1u16.to_le_bytes());
    pcm.extend_from_slice(&1u16.to_le_bytes());
    pcm.extend_from_slice(&48000u32.to_le_bytes());
    pcm.extend_from_slice(&96000u32.to_le_bytes());
    pcm.extend_from_slice(&2u16.to_le_bytes());
    pcm.extend_from_slice(&16u16.to_le_bytes());
    pcm.extend_from_slice(b"data");
    pcm.extend_from_slice(&bytes.to_le_bytes());
    pcm.resize(pcm.len() + bytes as usize, 0);
    std::fs::write(&wav, pcm).unwrap();
    let (cap, rx) = Capture::start(
        CaptureConfig {
            archive_dir: Path::new(&dir).join("archive").join("session-lab"),
            ..CaptureConfig::default()
        },
        CaptureConsent::after_setup(false, true),
    )
    .unwrap();
    let mut child = Command::new("/usr/bin/afplay").arg(wav).spawn().unwrap();
    drain(&rx, 2);
    println!("active_silence_cpu_one_core_percent={:.6}", drain(&rx, 8));
    let _ = child.kill();
    let _ = child.wait();
    drop(cap);
}
