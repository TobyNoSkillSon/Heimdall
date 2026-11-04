//! `clip::build` over a synthetic store: hot and cold-packed audio, a silent source, hardware HEVC screen epochs with
//! a coverage gap, a transcript line, and a writer holding the index's write lock throughout.
#![cfg(target_os = "macos")]
use heimdall_archive::{audio, clip, compact, store, video, DAY_NS};
use rusqlite::params;
use std::path::Path;

const S: u64 = 1_000_000_000;
/// The capture segment's wall anchor: host 0 is this Unix time.
const WALL: u64 = 1_800_000_000 * S;

/// Track types of an MP4/MOV, in file order, from its `trak/mdia/hdlr` boxes.
fn handlers(bytes: &[u8]) -> Vec<String> {
    fn boxes(b: &[u8]) -> Vec<(&[u8], &[u8])> {
        let mut out = vec![];
        let mut i = 0;
        while i + 8 <= b.len() {
            let mut size = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            let mut head = 8;
            if size == 1 && i + 16 <= b.len() {
                // 64-bit size (AVAssetWriter writes mdat this way).
                size = u64::from_be_bytes(b[i + 8..i + 16].try_into().unwrap()) as usize;
                head = 16;
            }
            if size < head || i + size > b.len() {
                break;
            }
            out.push((&b[i + 4..i + 8], &b[i + head..i + size]));
            i += size;
        }
        out
    }
    let mut out = vec![];
    for (kind, body) in boxes(bytes) {
        if kind != b"moov" {
            continue;
        }
        for (kind, trak) in boxes(body) {
            if kind != b"trak" {
                continue;
            }
            for (kind, mdia) in boxes(trak) {
                if kind != b"mdia" {
                    continue;
                }
                for (kind, hdlr) in boxes(mdia) {
                    if kind == b"hdlr" {
                        out.push(String::from_utf8_lossy(&hdlr[8..12]).into_owned());
                    }
                }
            }
        }
    }
    out
}

fn archive(root: &Path) {
    let db = store::open(root).unwrap();
    let session = store::ensure_session(&db, "session-clip").unwrap();
    let seg = store::create_segment(&db, session, "capture-clip", WALL, 0, "native").unwrap();
    for (id, meta) in [
        ("mic", r#"{"kind":{"InputDevice":{"name":"Test Mic","uid":"m"}}}"#),
        ("process:app", r#"{"kind":{"Process":{"name":"Player","bundle_id":"com.example.player","owner":{"name":"Player","bundle_id":"com.example.player"}}}}"#),
        ("process:call", r#"{"kind":{"Process":{"name":"Discord","bundle_id":"com.hnc.Discord","owner":{"name":"Discord","bundle_id":"com.hnc.Discord"}}}}"#),
    ] {
        db.execute("INSERT INTO sources VALUES(?1,?2,?3,0,NULL)", params![seg, id, meta]).unwrap();
    }
    // Mic: 48 kHz mono, one object a second, 10 ms callbacks with up to 0.2 ms of host-clock jitter; one loud
    // sample at host 1.500 s.
    for second in 0..4u64 {
        let pcm: Vec<f32> = (0..48_000u64).map(|i| if second * 48_000 + i == 72_000 { 0.9 } else { 0.01 * ((i as f32) * 0.05).sin() }).collect();
        let object = audio::put_hot(&db, 48_000, 1, &pcm).unwrap();
        for k in 0..100u64 {
            let host = second * S + k * 10_000_000 + (k % 3) * 100_000;
            db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',?2,48000,1,480,'f32-zstd',0,?3,?4,?5)", params![seg, host as i64, object, (k * 480) as i64, (WALL + host) as i64]).unwrap();
        }
    }
    // App: native 48 kHz stereo from host 1 s to 3 s, written long ago so it is compacted to the cold tier and its
    // rows packed into a cold chunk.
    let tone: Vec<f32> = (0..96_000).flat_map(|i| [0.3 * ((i as f32) * 0.03).sin(); 2]).collect();
    let object = audio::put_hot(&db, 48_000, 2, &tone).unwrap();
    for k in 0..200u64 {
        let host = S + k * 10_000_000;
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'process:app',?2,48000,2,480,'f32-zstd',0,?3,?4,?5)", params![seg, host as i64, object, (k * 480) as i64, (S + host) as i64]).unwrap();
    }
    // The call app was exact digital silence throughout.
    db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,wall_ns) VALUES(?1,'process:call',0,48000,2,192000,'zero',0,?2)", params![seg, WALL as i64]).unwrap();
    db.execute(
        "INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words) VALUES(?1,'u1',0,'mic','Test Mic','User',1400000000,2200000000,'hello there',?2)",
        params![session, r#"[{"text":"hello","start_ns":1400000000,"end_ns":1700000000},{"text":"there","start_ns":1750000000,"end_ns":2100000000}]"#],
    )
    .unwrap();
    drop(db);
    let now = 2 * DAY_NS + 10 * S;
    assert!(compact::compact_audio(root, now, 32_000, 8).unwrap().audio_groups >= 1);
    assert!(compact::compact_metadata(root, now, true).unwrap() >= 1, "the app's rows move into a cold chunk");
    // Screen: 5 fps hardware HEVC, nothing recorded between host 2.0 and 3.0 s.
    let writer = video::Writer::open(&store::session_path(root, "session-clip")).unwrap();
    let mut encoder = video::native::Encoder::new(64, 32, 5, 0.8).unwrap();
    let mut segment = None;
    for k in 0..20u64 {
        let host = k * 200_000_000;
        if (2 * S..3 * S).contains(&host) {
            continue;
        }
        let rgb: Vec<u8> = (0..64 * 32).flat_map(|_| [(k * 12) as u8, 90, 160]).collect();
        let (format, sample) = encoder.encode(&rgb, host + 1, host + 200_000_000).unwrap();
        let id = *segment.get_or_insert_with(|| writer.begin(&video::Geometry { display: 1, x: 0., y: 0., width: 64, height: 32 }, &format, host + 1, WALL + host, 5).unwrap());
        writer.append(id, &sample).unwrap();
    }
}

#[test]
fn a_window_becomes_one_movie_with_a_track_per_source_and_subtitles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("archive");
    archive(&root);
    // A live writer holds the index's write lock for the whole build: the clip reads past it.
    let busy = store::open(&root).unwrap();
    busy.execute_batch("BEGIN IMMEDIATE; INSERT INTO sessions(name,started_wall_ns) VALUES('session-busy',1);").unwrap();
    let before = std::fs::metadata(root.join(store::INDEX)).unwrap().len();

    let out = dir.path().join("clip.mp4");
    let reference = dir.path().join("reference");
    let mut options = clip::Options::new(WALL + S / 2, WALL + 3 * S + S / 2);
    options.reference_dir = Some(reference.clone());
    let report = clip::build(&root, &options, &out, &mut || true).unwrap();
    busy.execute_batch("ROLLBACK").unwrap();
    assert_eq!(std::fs::metadata(root.join(store::INDEX)).unwrap().len(), before, "the store is only read");

    let labels: Vec<&str> = report.audio.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(labels, ["Mic: Test Mic", "App: Player"], "{report:#?}");
    assert_eq!(report.silent, ["Call: Discord"]);
    assert!(report.audio[0].stored.contains_key("native 48000 Hz lossless"), "{:?}", report.audio[0].stored);
    assert!(report.audio[1].stored.keys().any(|k| k.starts_with("FLAC") || k.starts_with("Opus")), "{:?}", report.audio[1].stored);
    assert!(report.audio[0].max_offset_us <= 4000);
    assert_eq!((report.audio[1].rate, report.audio[1].channels), (48_000, 2));

    // The loud mic sample at host 1.500 s sits at clip 1.000 s (48,000), give or take the jitter the callbacks were
    // joined across.
    let mic = audio::pcm_from_bytes(&std::fs::read(reference.join("track-1.f32")).unwrap()).unwrap();
    assert_eq!(mic.len(), 3 * 48_000);
    let peak = mic.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0 as i64;
    assert!((peak - 48_000).abs() <= 10, "click at {peak}");
    // The cold app track sounds from clip 0.5 s to 2.5 s and is silent outside.
    let app = audio::pcm_from_bytes(&std::fs::read(reference.join("track-2.f32")).unwrap()).unwrap();
    let rms = |a: f64, b: f64| {
        let s = &app[(a * 96_000.) as usize..(b * 96_000.) as usize];
        (s.iter().map(|x| (x * x) as f64).sum::<f64>() / s.len() as f64).sqrt()
    };
    assert!(rms(0.0, 0.45) < 1e-6 && rms(0.6, 2.4) > 0.1 && rms(2.55, 3.0) < 1e-6, "{} {} {}", rms(0.0, 0.45), rms(0.6, 2.4), rms(2.55, 3.0));

    // Frames keep their capture times; the unrecorded second is black, never the held last frame.
    let frames: Vec<(u64, String)> = std::fs::read_to_string(reference.join("frames.txt"))
        .unwrap()
        .lines()
        .map(|l| {
            let (t, h) = l.split_once(' ').unwrap();
            (t.parse().unwrap(), h.to_owned())
        })
        .collect();
    assert!(frames.windows(2).all(|w| w[0].0 < w[1].0));
    assert!(frames.iter().any(|(t, h)| h == "black" && (1_400_000_000..1_600_000_000).contains(t)), "{frames:?}");
    let first = frames.iter().find(|(_, h)| h != "black").unwrap();
    assert_eq!(first.1.parse::<u64>().unwrap() + 1, 400_000_000 + 2, "the first frame after the window start");
    assert_eq!(report.video.black_gaps, 1);
    assert_eq!((report.video.width, report.video.height), (64, 32));

    let srt = std::fs::read_to_string(out.with_extension("srt")).unwrap();
    assert!(srt.contains("00:00:00,900 --> ") && srt.contains("User: hello there"), "{srt}");
    assert!(report.subtitles_embedded);
    assert_eq!(handlers(&std::fs::read(&out).unwrap()), ["vide", "soun", "soun", "sbtl"]);
    assert!(report.text().contains("Audio track 2: App: Player"));

    // A named display that recorded nothing still gives a (black) clip, and says so.
    let mut other = clip::Options::new(WALL + S / 2, WALL + S);
    other.display = Some(7);
    let r = clip::build(&root, &other, &dir.path().join("none.mov"), &mut || true).unwrap();
    assert!(r.notes.iter().any(|n| n.contains("no screen recording of display 7")), "{:?}", r.notes);
    assert_eq!(handlers(&std::fs::read(dir.path().join("none.mov")).unwrap())[0], "vide");

    // The app runs the bundled command with RFC 3339 times and reads its JSON report (crates/app/src/mcp_clip.rs).
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-clip"))
        .args(["--from", "2027-01-15T08:00:00.500000000Z", "--to", "2027-01-15T08:00:03.500000000Z", "--json", "--archive"])
        .arg(&root)
        .arg("--out")
        .arg(dir.path().join("cli.mov"))
        .output()
        .unwrap();
    assert!(cli.status.success(), "{}", String::from_utf8_lossy(&cli.stderr));
    let parsed: clip::Report = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(parsed.audio.len(), 2);
    assert_eq!(parsed.duration_ns, 3 * S);
    let again = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-clip"))
        .args(["--from", "2027-01-15T08:00:00Z", "--to", "+3s", "--archive"])
        .arg(&root)
        .arg("--out")
        .arg(dir.path().join("cli.mov"))
        .output()
        .unwrap();
    assert!(!again.status.success() && String::from_utf8_lossy(&again.stderr).contains("--force"), "an existing clip is never replaced silently");

    // A constant rate: one frame every half second, black on the ticks inside the hole.
    let mut fps = clip::Options::new(WALL + S / 2, WALL + 3 * S + S / 2);
    fps.fps = Some(2);
    fps.reference_dir = Some(dir.path().join("fps"));
    let r = clip::build(&root, &fps, &dir.path().join("fps.mp4"), &mut || true).unwrap();
    let ticks: Vec<(u64, String)> = std::fs::read_to_string(dir.path().join("fps/frames.txt")).unwrap().lines().map(|l| {
        let (t, h) = l.split_once(' ').unwrap();
        (t.parse().unwrap(), h.to_owned())
    }).collect();
    assert_eq!(ticks.iter().map(|t| t.0).collect::<Vec<_>>(), (0..6).map(|k| k * S / 2).collect::<Vec<_>>());
    assert_eq!(ticks[3].1, "black", "clip 1.5 s is host 2.0 s, inside the hole: {ticks:?}");
    assert_eq!(r.video.black_gaps, 1);

    // Cancelled after the first second: no movie, partial or claim is left behind.
    let mut calls = 0;
    let error = clip::build(&root, &clip::Options::new(WALL + S / 2, WALL + 3 * S), &dir.path().join("cancel.mp4"), &mut || {
        calls += 1;
        calls < 2
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    let left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.contains("cancel")).collect();
    assert!(left.is_empty(), "{left:?}");

    // A damaged screen sample and a damaged audio object cost their own stretch, not the clip.
    let (video_sha, audio_id): (String, String) = {
        let db = store::open(&root).unwrap();
        let v = db.query_row("SELECT sha256 FROM video_samples ORDER BY host_ns LIMIT 1 OFFSET 4", [], |r| r.get(0)).unwrap();
        let a = db.query_row("SELECT object_id FROM blocks WHERE source_id='mic' AND host_ns>=2000000000 LIMIT 1", [], |r| r.get(0)).unwrap();
        (v, a)
    };
    store::with_pack(&root, store::HOT, |p| {
        p.execute("UPDATE blobs SET data=x'00' WHERE key=?1", [video::key(&video_sha)])?;
        p.execute("UPDATE blobs SET data=x'00' WHERE key=?1", [store::audio_key(&audio_id)])?;
        Ok(())
    })
    .unwrap();
    let r = clip::build(&root, &clip::Options::new(WALL + S / 2, WALL + 3 * S + S / 2), &dir.path().join("damaged.mp4"), &mut || true).unwrap();
    assert!(r.notes.iter().any(|n| n.contains("could not be decoded and are black")), "{:?}", r.notes);
    assert!(r.notes.iter().any(|n| n.contains("Mic: Test Mic could not be read") && n.contains("silent")), "{:?}", r.notes);
    assert!(r.audio[0].stored.contains_key("unreadable"));
    assert_eq!(handlers(&std::fs::read(dir.path().join("damaged.mp4")).unwrap()), ["vide", "soun", "soun", "sbtl"]);
}
