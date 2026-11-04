#![cfg(target_os = "macos")]
// Explicit lab test only (ignored): needs the existing System Audio grant and plays a quiet two-second clip
// three times. Regular test runs never start capture or play audio.
use capture::{Archive, AppIdentity, Capture, CaptureConfig, CaptureConsent, CaptureEvent, SourceKind};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

const AFPLAY: &str = "/usr/bin/afplay";

struct Play {
    pid: u32,
    added: Option<capture::SourceId>,
    removed_reason: Option<String>,
    listed_playing: bool,
    gaps: Vec<String>,
}

/// Plays `wav` quietly for two seconds and watches capture until it ends.
fn play(rx: &capture::CaptureReceiver, playing: &crossbeam_channel::Receiver<Vec<AppIdentity>>, wav: &str) -> Play {
    let mut child = Command::new(AFPLAY).args(["-v", "0.1", "-t", "2", wav]).spawn().unwrap();
    let mut p = Play { pid: child.id(), added: None, removed_reason: None, listed_playing: false, gaps: vec![] };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(4) {
        while let Ok(apps) = playing.try_recv() {
            println!("  {:.3}s playing now: {:?}", started.elapsed().as_secs_f64(), apps.iter().map(|a| a.key.as_str()).collect::<Vec<_>>());
            p.listed_playing |= apps.iter().any(|a| a.key == AFPLAY);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(CaptureEvent::SourceAdded(info)) => {
                if matches!(info.kind, SourceKind::Process { pid, .. } if pid == p.pid) {
                    p.added = Some(info.id);
                } else if matches!(info.kind, SourceKind::Process { .. }) {
                    println!("other process source armed: {}", info.label);
                }
            }
            Ok(CaptureEvent::SourceRemoved { source_id, reason, .. }) if Some(&source_id) == p.added.as_ref() => {
                p.removed_reason = Some(reason)
            }
            Ok(CaptureEvent::Gap { detail, .. }) => p.gaps.push(detail),
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    p
}

#[test]
#[ignore = "explicit lab only; requires the System Audio grant and plays a quiet known WAV three times"]
fn unrecorded_apps_are_listed_not_tapped_and_recorded_apps_are_captured() {
    let dir = std::env::var("HEIMDALL_TAP_LAB_DIR").expect("isolated lab directory");
    let wav = std::env::var("HEIMDALL_TAP_LAB_WAV").expect("known 48-kHz PCM16 WAV");
    let (tx, playing) = crossbeam_channel::unbounded();
    let (capture, rx) = Capture::start(
        CaptureConfig { archive_dir: Path::new(&dir).join("session-lab"), playing_apps: Some(tx), ..CaptureConfig::default() },
        CaptureConsent::after_setup(false, true),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(1));

    // Default: nothing recorded. afplay is listed as playing but gets no tap.
    let off = play(&rx, &playing, &wav);
    println!("default: pid={} tapped={} listed_playing={} gaps={:?}", off.pid, off.added.is_some(), off.listed_playing, off.gaps);
    assert!(off.added.is_none(), "an unrecorded app was tapped");
    assert!(off.listed_playing, "the playing app was not reported for the menu");

    // Checked: the next afplay process is tapped (armed on its HAL process object) and archived.
    capture.set_recorded_apps(vec![AFPLAY.into()]);
    std::thread::sleep(Duration::from_millis(500));
    let on = play(&rx, &playing, &wav);
    println!("recorded: pid={} tapped={} listed_playing={} gaps={:?}", on.pid, on.added.is_some(), on.listed_playing, on.gaps);
    let id = on.added.clone().expect("a recorded app was not tapped");

    // Unchecked again: no tap for the next one.
    capture.set_recorded_apps(vec![]);
    std::thread::sleep(Duration::from_millis(500));
    let again = play(&rx, &playing, &wav);
    println!("unchecked: pid={} tapped={} listed_playing={}", again.pid, again.added.is_some(), again.listed_playing);
    assert!(again.added.is_none(), "an unchecked app was tapped");
    drop(capture);

    let archive = Archive::open_read_only(&Path::new(&dir).join("session-lab")).unwrap();
    let blocks = archive.read_blocks(&id).unwrap();
    let mono: Vec<f32> = blocks
        .iter()
        .flat_map(|b| b.samples.chunks_exact(b.format.channels as usize).map(|s| s.iter().sum::<f32>() / s.len() as f32))
        .collect();
    let nonzero = mono.iter().filter(|s| **s != 0.0).count();
    let peak = mono.iter().fold(0f32, |m, s| m.max(s.abs()));
    // Compare with the WAV's first 2 s at 8 kHz (scale-invariant: -v 0.1 only scales samples).
    let reference = wav_mono(Path::new(&wav));
    let reference: Vec<f32> = reference[..reference.len().min(96_000)].iter().step_by(6).copied().collect();
    let captured: Vec<f32> = mono.iter().step_by(6).copied().collect();
    let prefix = reference.len().min(2400);
    let best = (0..captured.len().saturating_sub(reference.len()).max(1))
        .map(|o| (o, similarity(&captured[o..(o + prefix).min(captured.len())], &reference[..prefix])))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    let end = (best.0 + reference.len()).min(captured.len());
    let correlation = similarity(&captured[best.0..end], &reference[..end - best.0]);
    println!(
        "recorded archive: frames={} nonzero={nonzero} peak={peak:.4} leading_ms={:.3} correlation={correlation:.6} removed={:?}",
        mono.len(),
        best.0 as f64 / 8.0,
        on.removed_reason
    );
    assert!(nonzero > 0 && correlation > 0.9, "recorded app audio missing or altered");
    let db = rusqlite::Connection::open_with_flags(Path::new(&dir).join("heimdall.sqlite"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut rows = db.prepare("SELECT id, metadata_json FROM sources").unwrap();
    let all: Vec<(String, String)> = rows.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
    println!("archived sources: {}", all.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>().join(", "));
    let others: Vec<_> = all
        .iter()
        .filter(|(_, meta)| [off.pid, again.pid].iter().any(|pid| meta.contains(&format!("\"pid\":{pid},"))))
        .collect();
    assert!(others.is_empty(), "unrecorded plays reached the archive: {others:?}");
}

fn wav_mono(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    let mut pos = 12;
    let mut channels = 0;
    while pos + 8 <= bytes.len() {
        let n = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let data = &bytes[pos + 8..pos + 8 + n];
        match &bytes[pos..pos + 4] {
            b"fmt " => channels = u16::from_le_bytes(data[2..4].try_into().unwrap()) as usize,
            b"data" => {
                return data
                    .chunks_exact(channels * 2)
                    .map(|f| f.as_chunks::<2>().0.iter().map(|s| i16::from_le_bytes(*s) as f32 / 32768.).sum::<f32>() / channels as f32)
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
