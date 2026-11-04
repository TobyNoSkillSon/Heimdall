use anyhow::Result;
use heimdall_archive::{
    store,
    timeline::{
        self,
        recording::{Context, Detector, Observation},
        ScreenChange, Search,
    },
    video,
};
use rusqlite::Connection;
use std::path::Path;
const S: u64 = 1_000_000_000;
fn context(app: &str, title: &str) -> Context {
    Context {
        app: Some(app.into()),
        window_title: title.into(),
        window_id: Some(1),
    }
}
fn chunks(db: &Connection) -> Result<Vec<timeline::Chunk>> {
    let mut cs = timeline::search(
        db,
        &Search {
            kinds: vec!["screen".into()],
            limit: 100,
            ..Default::default()
        },
    )?;
    cs.sort_by_key(|c| c.start_ns);
    Ok(cs)
}
fn writer(root: &Path) -> Result<video::Writer> {
    video::Writer::open(&root.join("session-screen-fixture"))
}
fn geometry() -> video::Geometry {
    video::Geometry {
        display: 1,
        x: 0.,
        y: 0.,
        width: 128,
        height: 72,
    }
}
fn png(rgb: &[u8]) -> Result<Vec<u8>> {
    let mut bytes = std::io::Cursor::new(vec![]);
    image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(128, 72, rgb.to_vec()).unwrap())
        .write_to(&mut bytes, image::ImageFormat::Png)?;
    Ok(bytes.into_inner())
}
fn format() -> video::Format {
    video::Format {
        width: 128,
        height: 72,
        hvcc: vec![],
        parameters: vec![],
    }
}
fn rgb(value: u8) -> Vec<u8> {
    vec![value; 128 * 72 * 3]
}
#[test]
fn signal_ignores_caret_and_codec_noise_and_accumulates_slow_scroll() -> Result<()> {
    let mut d = Detector::default();
    let c = context("browser", "page");
    let base = rgb(60);
    assert_eq!(
        d.observe(0, 128, 72, &base, c.clone())?.unwrap().change,
        ScreenChange::VisualChange
    );
    let mut caret = base.clone();
    caret[..24].fill(255);
    assert_eq!(
        d.observe(S, 128, 72, &caret, c.clone())?.unwrap().change,
        ScreenChange::Unchanged
    );
    assert!(d
        .observe(S + S / 5, 128, 72, &rgb(220), c.clone())?
        .is_none());
    for n in 2..6 {
        assert_eq!(
            d.observe(n * S, 128, 72, &rgb(60 + (n * 4) as u8), c.clone())?
                .unwrap()
                .change,
            ScreenChange::Unchanged
        );
    }
    assert_eq!(
        d.observe(6 * S, 128, 72, &rgb(84), c.clone())?
            .unwrap()
            .change,
        ScreenChange::VisualChange
    );
    assert_eq!(
        d.idle(7 * S, context("browser", "new page"))
            .unwrap()
            .change,
        ScreenChange::PageLoad
    );
    assert_eq!(
        d.idle(8 * S, context("editor", "file")).unwrap().change,
        ScreenChange::AppSwitch
    );
    let mut other_window = context("editor", "file");
    other_window.window_id = Some(2);
    assert_eq!(
        d.idle(9 * S, other_window).unwrap().change,
        ScreenChange::PageLoad
    );
    // A real screenshot fixture: kept out of the public source export, where this part is skipped.
    let Ok(png) = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../decide/tests/fixtures/screen.png")) else {
        eprintln!("screen.png fixture missing; scroll checks skipped");
        return Ok(());
    };
    let screen = image::load_from_memory(&png)?.to_rgb8();
    let mut scrolled = screen.clone();
    for y in 0..256 {
        for x in 0..256 {
            scrolled.put_pixel(x, y, *screen.get_pixel(x, (y + 96) % 256));
        }
    }
    let mut d = Detector::default();
    d.observe(0, 256, 256, screen.as_raw(), c.clone())?;
    assert_eq!(
        d.observe(S, 256, 256, scrolled.as_raw(), c)?
            .unwrap()
            .change,
        ScreenChange::VisualChange
    );
    Ok(())
}
#[test]
fn recorded_png_coverage_caps_idle_references_and_backfill_resume() -> Result<()> {
    let t = tempfile::tempdir()?;
    let w = writer(t.path())?;
    let id = w.begin(&geometry(), &format(), 0, 10 * S, 5)?;
    let a = video::Sample {
        host_ns: 0,
        end_ns: S / 5,
        key: true,
        data: png(&rgb(40))?,
    };
    w.append_screen(
        id,
        &a,
        Some(&Observation {
            context: context("browser", "page"),
            change: ScreenChange::VisualChange,
        }),
    )?;
    // An idle interval crossing the cap must use the covered sample, not new pixels.
    w.extend_screen(
        id,
        46 * S,
        Some(&Observation {
            context: context("browser", "page"),
            change: ScreenChange::Unchanged,
        }),
    )?;
    let b = video::Sample {
        host_ns: 46 * S,
        end_ns: 47 * S,
        key: true,
        data: png(&rgb(220))?,
    };
    w.append_screen(
        id,
        &b,
        Some(&Observation {
            context: context("editor", "file"),
            change: ScreenChange::AppSwitch,
        }),
    )?;
    w.close(id)?;
    w.gap(1, 47 * S, "off")?;
    let next = w.begin(&geometry(), &format(), 50 * S, 60 * S, 5)?;
    w.append_screen(
        next,
        &video::Sample {
            host_ns: 50 * S,
            end_ns: 51 * S,
            ..b.clone()
        },
        Some(&Observation {
            context: context("editor", "file"),
            change: ScreenChange::Unchanged,
        }),
    )?;
    w.close(next)?;
    let db = store::open(t.path())?;
    let before = chunks(&db)?;
    assert_eq!(before.len(), 4);
    assert_eq!((before[0].start_ns, before[0].end_ns), (0, 45 * S));
    assert_eq!((before[1].start_ns, before[1].end_ns), (45 * S, 46 * S));
    assert_eq!(before[1].details["representative"]["host_ns"], 0);
    assert!(before
        .iter()
        .all(|c| c.end_ns - c.start_ns <= 45 * S && c.links.iter().all(|l| l.relation != "image")));
    assert!(before[2]
        .tags
        .iter()
        .any(|t| t.key == "window_title" && t.value == "file"));
    let sid: i64 = db.query_row(
        "SELECT session FROM video_segments WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    // Replay preserved recording metadata with a gate stopping between epochs.
    db.execute("DELETE FROM timeline_video_backfill", [])?;
    for table in ["timeline_chunks", "chunk_tags", "chunk_links"] {
        db.execute(&format!("DELETE FROM {table}"), [])?;
    }
    assert_eq!(timeline::recording::backfill(t.path(), &mut || false)?, 0);
    assert_eq!(timeline::recording::backfill(t.path(), &mut || true)?, 1);
    assert_eq!(timeline::recording::backfill(t.path(), &mut || true)?, 1);
    assert_eq!(timeline::recording::backfill(t.path(), &mut || true)?, 0);
    let after = chunks(&db)?;
    assert_eq!(
        before
            .iter()
            .map(|c| (&c.id, c.start_ns, c.end_ns, &c.tags, &c.links))
            .collect::<Vec<_>>(),
        after
            .iter()
            .map(|c| (&c.id, c.start_ns, c.end_ns, &c.tags, &c.links))
            .collect::<Vec<_>>()
    );
    assert!(video::at(&t.path().join("session-screen-fixture"), 1, 48 * S)?.is_none());
    assert_eq!(sid, after[0].session);
    Ok(())
}
#[cfg(target_os = "macos")]
#[test]
fn recorded_hevc_legacy_backfill_detects_content_and_preserves_rotation() -> Result<()> {
    let t = tempfile::tempdir()?;
    let w = writer(t.path())?;
    // Real hardware-encoded recording samples; backfill must decode their committed blobs.
    let mut encoder = video::native::Encoder::new(128, 72, 5, 0.8)?;
    let mut id = 0;
    for i in 0..300 {
        let ns = i * S / 5;
        let pixels = rgb(if i < 15 { 40 } else { 210 });
        let (f, s) = encoder.encode(&pixels, ns, ns + S / 5)?;
        if id == 0 {
            id = w.begin(&geometry(), &f, ns, 10 * S, 5)?;
        }
        w.append(id, &s)?;
    }
    w.extend(id, 60 * S)?;
    w.close(id)?;
    let mut encoder = video::native::Encoder::new(128, 72, 5, 0.8)?;
    let (f, s) = encoder.encode(&rgb(210), 60 * S, 61 * S)?;
    let next = w.begin(&geometry(), &f, 60 * S, 70 * S, 5)?;
    w.append(next, &s)?;
    w.extend(next, 90 * S)?;
    w.close(next)?;
    let mut admission = 0;
    assert_eq!(
        timeline::recording::backfill(t.path(), &mut || {
            admission += 1;
            admission < 5
        })?,
        0
    );
    let db = store::open(t.path())?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM timeline_video_backfill", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    loop {
        let mut remaining = 256usize;
        let n = timeline::recording::backfill(t.path(), &mut || {
            if remaining == 0 {
                return false;
            }
            remaining -= 1;
            true
        })?;
        if n == 0 {
            break;
        }
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM timeline_video_backfill", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    let cs = chunks(&db)?;
    assert_eq!(
        cs.iter()
            .map(|c| (c.start_ns, c.end_ns))
            .collect::<Vec<_>>(),
        vec![(0, 3 * S), (3 * S, 48 * S), (48 * S, 90 * S)]
    );
    assert!(cs.iter().all(|c| !c
        .tags
        .iter()
        .any(|t| t.key == "app" || t.key == "window_title")));
    for c in cs {
        let stamp = c.details["representative"]["host_ns"].as_u64().unwrap();
        let (s, frames) = video::at(&t.path().join("session-screen-fixture"), 1, stamp)?.unwrap();
        let mut d = video::native::Decoder::new(&s.format)?;
        for frame in frames {
            assert_eq!(d.decode(&frame)?.len(), 128 * 72 * 3);
        }
    }
    Ok(())
}
#[test]
fn schema_upgrade_retains_existing_tags_and_failed_recording_rolls_back_projection() -> Result<()> {
    let t = tempfile::tempdir()?;
    let w = writer(t.path())?;
    let db = store::open(t.path())?;
    db.execute_batch("DROP TABLE chunk_tags; CREATE TABLE chunk_tags(chunk_id TEXT,key TEXT CHECK(key IN ('app')),value TEXT,PRIMARY KEY(chunk_id,key,value)) WITHOUT ROWID; INSERT INTO chunk_tags VALUES('old','app','browser');")?;
    // A distinct connection initialization path emulates an old on-disk schema.
    // schema runs inside store startup; test the exact migration via a subprocess-free copied root.
    drop(db);
    let upgraded = tempfile::tempdir()?;
    let source = store::open(t.path())?;
    let mut dest = rusqlite::Connection::open(upgraded.path().join("heimdall.sqlite"))?;
    rusqlite::backup::Backup::new(&source, &mut dest)?.run_to_completion(
        100,
        std::time::Duration::from_millis(1),
        None,
    )?;
    drop(dest);
    let db = store::open(upgraded.path())?;
    assert_eq!(
        db.query_row(
            "SELECT value FROM chunk_tags WHERE chunk_id='old'",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "browser"
    );
    db.execute(
        "INSERT INTO chunk_tags VALUES('new','window_title','page')",
        [],
    )?;
    let id = w.begin(&geometry(), &format(), 0, 10 * S, 5)?;
    let db = store::open(t.path())?;
    db.execute_batch("CREATE TRIGGER refuse_event BEFORE INSERT ON video_screen_events BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")?;
    assert!(w
        .append_screen(
            id,
            &video::Sample {
                host_ns: 0,
                end_ns: S,
                key: true,
                data: png(&rgb(40))?
            },
            Some(&Observation {
                context: Context::default(),
                change: ScreenChange::VisualChange
            })
        )
        .is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM video_samples", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "agent-only one-minute accelerated HEVC recording/backfill CPU probe"]
fn recorded_hevc_backfill_cpu() -> Result<()> {
    fn cpu() -> f64 {
        unsafe {
            let mut r = std::mem::zeroed::<libc::rusage>();
            assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut r), 0);
            r.ru_utime.tv_sec as f64
                + r.ru_utime.tv_usec as f64 / 1e6
                + r.ru_stime.tv_sec as f64
                + r.ru_stime.tv_usec as f64 / 1e6
        }
    }
    let t = tempfile::tempdir()?;
    let w = writer(t.path())?;
    let mut encoder = video::native::Encoder::new(1728, 1116, 5, 0.8)?;
    let mut id = 0;
    for i in 0..300u64 {
        let ns = i * S / 5;
        let pixels = vec![if i / 50 % 2 == 0 { 40 } else { 210 }; 1728 * 1116 * 3];
        let (f, s) = encoder.encode(&pixels, ns, ns + S / 5)?;
        if i == 0 {
            id = w.begin(
                &video::Geometry {
                    width: 1728,
                    height: 1116,
                    ..geometry()
                },
                &f,
                0,
                10 * S,
                5,
            )?;
        }
        w.append(id, &s)?;
    }
    w.close(id)?;
    let start = cpu();
    assert_eq!(timeline::recording::backfill(t.path(), &mut || true)?, 1);
    let elapsed = cpu() - start;
    let db = store::open(t.path())?;
    let mut rows = 0i64;
    for table in [
        "timeline_chunks",
        "chunk_tags",
        "chunk_links",
        "video_screen_events",
        "timeline_video_backfill",
        "timeline_screen_detector",
    ] {
        rows += db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })?;
    }
    println!("SCREEN_BACKFILL 1728x1116_5fps_60s cpu_s_per_recorded_hour={:.6} derived_rows_per_hour={} states={}",elapsed*60.,rows*60,chunks(&db)?.len());
    Ok(())
}

#[test]
fn cadence_jitter_preserves_state_but_a_recorded_gap_splits_it() -> Result<()> {
    let t = tempfile::tempdir()?;
    let w = writer(t.path())?;
    let id = w.begin(&geometry(), &format(), 0, 10 * S, 5)?;
    for ns in [0, 205_000_000, 409_000_000, 613_000_000] {
        w.append_screen(
            id,
            &video::Sample {
                host_ns: ns,
                end_ns: ns + S / 5,
                key: true,
                data: png(&rgb(40))?,
            },
            Some(&Observation {
                context: context("browser", "page"),
                change: if ns == 0 {
                    ScreenChange::VisualChange
                } else {
                    ScreenChange::Unchanged
                },
            }),
        )?;
    }
    w.close(id)?;
    let db = store::open(t.path())?;
    assert_eq!(chunks(&db)?.len(), 1);
    w.gap(1, 813_000_000, "off")?;
    let next = w.begin(&geometry(), &format(), 820_000_000, 10 * S + 820_000_000, 5)?;
    w.append_screen(
        next,
        &video::Sample {
            host_ns: 820_000_000,
            end_ns: 1_020_000_000,
            key: true,
            data: png(&rgb(40))?,
        },
        Some(&Observation {
            context: context("browser", "page"),
            change: ScreenChange::Unchanged,
        }),
    )?;
    w.close(next)?;
    assert_eq!(chunks(&db)?.len(), 2);
    Ok(())
}

#[cfg(target_os="macos")]
#[test]
fn recorded_representatives_prepare_screen_and_moment_hot_and_cold() -> Result<()> {
    use heimdall_archive::embedding::{prepare_chunk,Part};
    let t=tempfile::tempdir()?;let w=writer(t.path())?;
    let mut encoder=video::native::Encoder::new(128,72,5,0.8)?;
    let mut id=0;
    for ns in [0,S/5,2*S/5,3*S/5,4*S/5,S,3*S,3*S+S/5] {
        let (f,s)=encoder.encode(&rgb(80),ns,ns+S/5)?;
        if id==0 {id=w.begin(&geometry(),&f,ns,1,5)?;}
        let o=matches!(ns,0|400_000_000).then(||Observation{context:context("browser","page"),change:ScreenChange::VisualChange});
        w.append_screen(id,&s,o.as_ref())?;
    }
    w.close(id)?;
    let db=store::open(t.path())?;
    let cs=chunks(&db)?;
    assert!(cs.iter().any(|c|c.details["representative"]["host_ns"]==2*S/5));
    for cold in [false,true] {
        if cold {
            // Activity resumes after admission: cancel during re-encoding, retain hot refs.
            let mut checks=0;
            assert_eq!(heimdall_archive::compact::compact_video_while(t.path(),heimdall_archive::DAY_NS+10*S,&mut ||true,&mut ||{checks+=1;checks<4})?,0);
            assert_eq!(video::segments(t.path(),cs[0].session)?[0].tier,"hot");
            assert_eq!(heimdall_archive::compact::compact_video_while(t.path(),heimdall_archive::DAY_NS+10*S,&mut ||true,&mut ||true)?,1);
            assert_eq!(video::segments(t.path(),cs[0].session)?[0].tier,"archive");
        }
        for c in &cs {
            let mut moment=c.clone();moment.kind="moment".into();
            moment.links=vec![timeline::Link{relation:"member".into(),target:c.id.clone()}];
            for input in [c,&moment] {
                let out=tempfile::tempdir()?;
                let prepared=prepare_chunk(t.path(),input,out.path())?;
                let paths:Vec<_>=prepared.iter().flat_map(|i|&i.parts).filter_map(|p|if let Part::Image{path}=p {Some(path)}else{None}).collect();
                assert_eq!(paths.len(),1);
                let image=image::open(paths[0])?.to_rgb8();
                assert_eq!(image.dimensions(),(128,72));
                assert!(image.as_raw().iter().all(|v|v.abs_diff(80)<20));
            }
        }
        // Tiering preserves the explicit uncovered interval inside this epoch.
        assert!(video::at(&t.path().join("session-screen-fixture"),1,2*S)?.is_none());
    }
    Ok(())
}
