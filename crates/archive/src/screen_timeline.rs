//! Model-free recording projection. Only boundary metadata and video references are stored.
use super::*;
use crate::video;

pub const SAMPLE_NS: u64 = 1_000_000_000;
const FLUSH_NS: u64 = 5_000_000_000;
const CELLS: usize = 32 * 18;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub app: Option<String>,
    pub window_title: String,
    #[serde(default)]
    pub window_id: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub context: Context,
    pub change: ScreenChange,
}
/// 576 RGB cells, four point samples each. Compare against the last accepted state,
/// so slow scrolling accumulates rather than disappearing in adjacent-frame noise.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Detector {
    next_ns: u64,
    baseline: Option<Vec<u8>>,
    context: Context,
    size: (u32, u32),
}
impl Detector {
    pub fn context(&self) -> &Context {
        &self.context
    }
    pub fn due(&self, ns: u64) -> bool {
        self.baseline.is_none() || ns >= self.next_ns
    }
    pub fn observe(
        &mut self,
        ns: u64,
        width: u32,
        height: u32,
        rgb: &[u8],
        context: Context,
    ) -> Result<Option<Observation>> {
        if !self.due(ns) {
            return Ok(None);
        }
        ensure!(
            width > 0 && height > 0 && rgb.len() == width as usize * height as usize * 3,
            "screen signal geometry"
        );
        let mut grid = vec![0u8; CELLS * 3];
        for y in 0..18 {
            for x in 0..32 {
                for c in 0..3 {
                    let mut sum = 0u32;
                    for dy in [1, 3] {
                        for dx in [1, 3] {
                            let px =
                                ((x * 4 + dx) * width as usize / (32 * 4)).min(width as usize - 1);
                            let py = ((y * 4 + dy) * height as usize / (18 * 4))
                                .min(height as usize - 1);
                            sum += rgb[(py * width as usize + px) * 3 + c] as u32;
                        }
                    }
                    grid[(y * 32 + x) * 3 + c] = (sum / 4) as u8;
                }
            }
        }
        let change = if self.baseline.is_none() || self.size != (width, height) {
            ScreenChange::VisualChange
        } else if self.context.app != context.app {
            ScreenChange::AppSwitch
        } else if self.context.window_title != context.window_title
            || self.context.window_id != context.window_id
        {
            ScreenChange::PageLoad
        } else {
            let changed = grid
                .chunks_exact(3)
                .zip(self.baseline.as_ref().unwrap().chunks_exact(3))
                .filter(|(a, b)| {
                    a.iter()
                        .zip(*b)
                        .map(|(a, b)| a.abs_diff(*b) as u32)
                        .sum::<u32>()
                        >= 72
                })
                .count();
            if changed * 100 >= CELLS * 15 {
                ScreenChange::VisualChange
            } else {
                ScreenChange::Unchanged
            }
        };
        if change != ScreenChange::Unchanged {
            self.baseline = Some(grid);
        }
        self.size = (width, height);
        self.context = context.clone();
        self.next_ns = ns.saturating_add(SAMPLE_NS);
        Ok(Some(Observation { context, change }))
    }
    pub fn idle(&mut self, ns: u64, context: Context) -> Option<Observation> {
        if !self.due(ns) {
            return None;
        }
        self.next_ns = ns.saturating_add(SAMPLE_NS);
        let change = if self.context.app != context.app {
            ScreenChange::AppSwitch
        } else if self.context.window_title != context.window_title
            || self.context.window_id != context.window_id
        {
            ScreenChange::PageLoad
        } else {
            ScreenChange::Unchanged
        };
        self.context = context.clone();
        Some(Observation { context, change })
    }
}
pub(crate) fn schema(db: &Connection) -> Result<()> {
    // Existing chunk indexes have a constrained tag key list; preserve all rows.
    let sql: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE name='chunk_tags'",
        [],
        |r| r.get(0),
    )?;
    if !sql.contains("'window_title'") {
        transaction(db, || {
            db.execute_batch("ALTER TABLE chunk_tags RENAME TO chunk_tags_old;
                CREATE TABLE chunk_tags(chunk_id TEXT NOT NULL,key TEXT NOT NULL CHECK(key IN ('speaker','source','app','language','command_kind','agent','listening','window_title')),value TEXT NOT NULL,PRIMARY KEY(chunk_id,key,value)) WITHOUT ROWID;
                INSERT INTO chunk_tags SELECT * FROM chunk_tags_old; DROP TABLE chunk_tags_old;
                CREATE INDEX tag_lookup ON chunk_tags(key,value,chunk_id);")?;
            Ok(())
        })?;
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS video_screen_events(segment INTEGER NOT NULL,host_ns INTEGER NOT NULL,context TEXT NOT NULL,change TEXT NOT NULL,PRIMARY KEY(segment,host_ns)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS timeline_video_backfill(segment INTEGER PRIMARY KEY,end_ns INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS timeline_screen_detector(session INTEGER NOT NULL,display INTEGER NOT NULL,end_ns INTEGER NOT NULL,state TEXT NOT NULL,PRIMARY KEY(session,display)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS video_gap_time ON video_gaps(session,display,host_ns);")?;
    Ok(())
}
/// Inside the recording's existing transaction. No extra fsync or pixel copy.
pub(crate) fn observe(
    db: &Connection,
    segment: i64,
    ns: u64,
    observation: &Observation,
) -> Result<()> {
    let first: bool = db.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM video_screen_events WHERE segment=?1)",
        [segment],
        |r| r.get(0),
    )?;
    if first || observation.change != ScreenChange::Unchanged {
        db.execute(
            "INSERT OR IGNORE INTO video_screen_events VALUES(?1,?2,?3,?4)",
            params![
                segment,
                ns as i64,
                serde_json::to_string(&observation.context)?,
                serde_json::to_string(&observation.change)?
            ],
        )?;
    }
    let end: u64 = db.query_row(
        "SELECT end_ns FROM video_segments WHERE id=?1",
        [segment],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    )?;
    let through: Option<u64> = db
        .query_row(
            "SELECT end_ns FROM timeline_video_backfill WHERE segment=?1",
            [segment],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )
        .optional()?;
    if first
        || observation.change != ScreenChange::Unchanged
        || through.is_none_or(|n| end.saturating_sub(n) >= FLUSH_NS)
    {
        project(db, segment)?;
    }
    Ok(())
}
/// Replay committed coverage and boundary events only. Checkpoint and chunks publish atomically.
pub(crate) fn project(db: &Connection, segment: i64) -> Result<()> {
    let (session, display, start, wall, end, fps): (i64, u32, u64, u64, u64, u32) = db.query_row(
        "SELECT session,display,start_ns,wall_ns,end_ns,fps FROM video_segments WHERE id=?1",
        [segment],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
                r.get::<_, i64>(4)? as u64,
                r.get(5)?,
            ))
        },
    )?;
    let through: u64 = db
        .query_row(
            "SELECT end_ns FROM timeline_video_backfill WHERE segment=?1",
            [segment],
            |r| Ok(r.get::<_, i64>(0)? as u64),
        )
        .optional()?
        .unwrap_or(start);
    let events:Vec<(u64,Context,ScreenChange)>=db.prepare("SELECT host_ns,context,change FROM video_screen_events WHERE segment=?1 ORDER BY host_ns")?.query_map([segment],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.map(|r|{let (n,c,k)=r?;Ok((n,serde_json::from_str(&c)?,serde_json::from_str(&k)?))}).collect::<Result<_>>()?;
    let spans:Vec<(u64,u64)>=db.prepare("SELECT host_ns,end_ns FROM video_samples WHERE segment=?1 AND end_ns>?2 ORDER BY host_ns")?.query_map(params![segment,through as i64],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64)))?.collect::<rusqlite::Result<_>>()?;
    let tolerance = 1_000_000_000 / fps as u64 / 2;
    // Merge ordinary cadence jitter; explicit recording gaps still split states.
    // Merge contiguous sample coverage before touching chunks (one update per state).
    let mut runs: Vec<(u64, u64)> = vec![];
    for (a, b) in spans {
        let a = a.max(through);
        if b <= a {
            continue;
        }
        if let Some(last) = runs
            .last_mut()
            .filter(|last| a.saturating_sub(last.1) <= tolerance)
        {
            last.1 = b;
        } else {
            runs.push((a, b));
        }
    }
    for (a, b) in runs {
        let mut at = a;
        while at < b {
            let event = events.iter().rev().find(|e| e.0 <= at);
            let context = event.map(|e| e.1.clone()).unwrap_or_default();
            let change = event
                .filter(|e| e.0 == at)
                .map(|e| e.2.clone())
                .unwrap_or(ScreenChange::Unchanged);
            let to = events
                .iter()
                .find(|e| e.0 > at)
                .map(|e| e.0.min(b))
                .unwrap_or(b);
            span(
                db,
                session,
                display,
                segment,
                at,
                to,
                wall + at - start,
                &context,
                change,
                tolerance,
            )?;
            at = to;
        }
    }
    db.execute("INSERT INTO timeline_video_backfill VALUES(?1,?2) ON CONFLICT(segment) DO UPDATE SET end_ns=excluded.end_ns",params![segment,end as i64])?;
    // Codec rotation alone does not close the state. A following gap/coverage break does.
    Ok(())
}
fn span(
    db: &Connection,
    session: i64,
    display: u32,
    segment: i64,
    from: u64,
    to: u64,
    wall: u64,
    context: &Context,
    mut change: ScreenChange,
    tolerance: u64,
) -> Result<()> {
    let source = format!("display:{display}");
    let mut at = from;
    while at < to {
        let prior:Option<String>=db.query_row("SELECT id FROM timeline_chunks WHERE session=?1 AND source=?2 AND kind='screen' AND start_ns<=?3 ORDER BY start_ns DESC LIMIT 1",params![session,source,at as i64],|r|r.get(0)).optional()?;
        let mut old = prior.map(|id| load(db, &id)).transpose()?;
        let gap:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM video_gaps WHERE session=?1 AND display=?2 AND host_ns>=?3 AND host_ns<=?4)",params![session,display,old.as_ref().map(|c|c.end_ns).unwrap_or(at) as i64,at as i64],|r|r.get(0))?;
        let extend = !gap
            && old.as_ref().is_some_and(|c| {
                c.end_ns <= at
                    && at - c.end_ns <= tolerance
                    && c.details["closed"] != true
                    && at < c.start_ns + SCREEN_MAX_NS
                    && change == ScreenChange::Unchanged
                    && c.details["context"] == json!(context)
            });
        let mut c = if extend {
            old.take().unwrap()
        } else {
            if let Some(mut c) = old {
                if c.details["closed"] != true {
                    c.details["closed"] = json!(true);
                    put(db, &mut c)?;
                    moments(db, &c)?;
                }
            }
            let rep:u64=db.query_row("SELECT host_ns FROM video_samples WHERE segment=?1 AND host_ns<=?2 AND end_ns>?3 ORDER BY host_ns DESC LIMIT 1",params![segment,at as i64,at.saturating_sub(tolerance) as i64],|r|Ok(r.get::<_,i64>(0)? as u64))?;
            let mut c = Chunk::new(
                session,
                "screen",
                "state",
                &source,
                &format!("{source}:{at}"),
                at,
                at,
                wall + at - from,
            );
            c.details = json!({"window_title":context.window_title,"context":context,"change":change,"representative":{"segment":segment,"host_ns":rep}});
            c.link("video_segment", segment);
            c.link(
                "video_frame",
                json!({"segment":segment,"host_ns":rep}).to_string(),
            );
            if let Some(app) = &context.app {
                c.tag("app", app);
            }
            c.tag("window_title", &context.window_title);
            base_tags(db, &mut c)?;
            c
        };
        c.end_ns = to.min(c.start_ns + SCREEN_MAX_NS);
        c.end_wall_ns = c.start_wall_ns + c.end_ns - c.start_ns;
        if c.end_ns == c.start_ns + SCREEN_MAX_NS {
            c.details["closed"] = json!(true);
        }
        at = c.end_ns;
        put(db, &mut c)?;
        moments(db, &c)?;
        change = ScreenChange::Unchanged;
    }
    Ok(())
}
pub(crate) fn stop(db: &Connection, session: i64, display: u32) -> Result<()> {
    let source = format!("display:{display}");
    let id:Option<String>=db.query_row("SELECT id FROM timeline_chunks WHERE session=?1 AND source=?2 AND kind='screen' ORDER BY start_ns DESC LIMIT 1",params![session,source],|r|r.get(0)).optional()?;
    if let Some(id) = id {
        let mut c = load(db, &id)?;
        c.details["closed"] = json!(true);
        put(db, &mut c)?;
        moments(db, &c)?;
    }
    Ok(())
}
/// At most one <=60 s codec epoch per call, no whole-session decode or RGB history.
pub fn backfill(root: &Path, more: &mut impl FnMut() -> bool) -> Result<usize> {
    if !more() {
        return Ok(0);
    }
    let _reader = crate::compact::reader_lock(root)?;
    let db = store::open(root)?;
    let id:Option<i64>=db.query_row("SELECT s.id FROM video_segments s LEFT JOIN timeline_video_backfill b ON b.segment=s.id WHERE s.closed=1 AND coalesce(b.end_ns,-1)<s.end_ns ORDER BY s.session,s.display,s.start_ns,s.id LIMIT 1",[],|r|r.get(0)).optional()?;
    let Some(id) = id else { return Ok(0) };
    let segment=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE id=?1",[id],video::row)?;
    let known: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM video_screen_events WHERE segment=?1)",
        [id],
        |r| r.get(0),
    )?;
    let mut events = vec![];
    let mut detector_state = None;
    if !known {
        let previous:Option<String>=db.query_row("SELECT state FROM timeline_screen_detector WHERE session=?1 AND display=?2 AND end_ns=?3 AND NOT EXISTS(SELECT 1 FROM video_gaps WHERE session=?1 AND display=?2 AND host_ns=?3)",params![segment.session,segment.geometry.display,segment.start_ns as i64],|r|r.get(0)).optional()?;
        let seeded = previous.is_some();
        let mut detector = previous
            .map(|s| serde_json::from_str::<Detector>(&s))
            .transpose()?
            .unwrap_or_default();
        #[cfg(target_os = "macos")]
        let mut decoder = if segment.format.parameters.is_empty() {
            None
        } else {
            Some(video::native::Decoder::new(&segment.format)?)
        };
        for sample in video::samples(root, id, u64::MAX.min(i64::MAX as u64))? {
            let due = detector.due(sample.host_ns);
            // The caller's 256-record background budget counts analyzed frames,
            // not codec references (up to 30/s). Recheck before every one-Hz
            // analysis and before publication; at most one second of codec
            // references advances between admission checks.
            if due && !more() {
                return Ok(0);
            }
            let rgb = if segment.format.parameters.is_empty() {
                if !due {
                    continue;
                }
                image::load_from_memory(&sample.data)?.to_rgb8().into_raw()
            } else {
                #[cfg(target_os = "macos")]
                {
                    let d = decoder.as_mut().unwrap();
                    if due {
                        d.decode(&sample)?
                    } else {
                        d.advance(&sample)?;
                        continue;
                    }
                }
                #[cfg(not(target_os = "macos"))]
                anyhow::bail!("HEVC backfill requires macOS");
            };
            if let Some(o) = detector.observe(
                sample.host_ns,
                segment.format.width,
                segment.format.height,
                &rgb,
                Context::default(),
            )? {
                if events.is_empty() || o.change != ScreenChange::Unchanged {
                    if events.is_empty() && !seeded {
                        events.push((
                            sample.host_ns,
                            Observation {
                                change: ScreenChange::Unchanged,
                                ..o
                            },
                        ));
                    } else {
                        events.push((sample.host_ns, o));
                    }
                }
            }
        }
        detector_state = Some(serde_json::to_string(&detector)?);
    }
    if !more() {
        return Ok(0);
    }
    transaction(&db, || {
        for (ns, o) in events {
            db.execute(
                "INSERT OR IGNORE INTO video_screen_events VALUES(?1,?2,?3,?4)",
                params![
                    id,
                    ns as i64,
                    serde_json::to_string(&o.context)?,
                    serde_json::to_string(&o.change)?
                ],
            )?;
        }
        project(&db, id)?;
        if let Some(state) = detector_state {
            db.execute("INSERT INTO timeline_screen_detector VALUES(?1,?2,?3,?4) ON CONFLICT(session,display) DO UPDATE SET end_ns=excluded.end_ns,state=excluded.state",params![segment.session,segment.geometry.display,segment.end_ns as i64,state])?;
        }
        Ok(())
    })?;
    Ok(1)
}

#[cfg(test)]
mod cost {
    use super::*;
    fn cpu() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        unsafe {
            assert_eq!(libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()), 0);
            let r = usage.assume_init();
            r.ru_utime.tv_sec as f64
                + r.ru_utime.tv_usec as f64 / 1e6
                + r.ru_stime.tv_sec as f64
                + r.ru_stime.tv_usec as f64 / 1e6
        }
    }
    #[test]
    #[ignore = "agent-only short CPU probe, run alone with --ignored --nocapture"]
    fn recorded_hour_incremental_cpu_and_rows() -> Result<()> {
        let t = tempfile::tempdir()?;
        let db = store::open(t.path())?;
        let session = store::ensure_session(&db, "session-cost")?;
        let pixels = vec![60u8; 1728 * 1116 * 3];
        for changing in [false, true] {
            let mut timings = vec![];
            let mut rows = 0;
            let mut states = 0;
            for enabled in [false, true, true, false] {
                db.execute_batch("BEGIN")?;
                let mut detector = Detector::default();
                let begin = cpu();
                for i in 0..18000u64 {
                    let ns = i * 200_000_000;
                    let epoch = (i / 300 + 1) as i64;
                    if i % 300 == 0 {
                        if enabled && i > 0 {
                            project(&db, epoch - 1)?;
                        }
                        db.execute("INSERT INTO video_segments(id,session,display,geometry,format,start_ns,end_ns,wall_ns,fps,tier) VALUES(?1,?2,1,'{}','{}',?3,?3,?4,5,'hot')",params![epoch,session,ns as i64,(1_800_000_000_000_000_000u64+ns) as i64])?;
                    }
                    db.prepare_cached(
                        "INSERT INTO video_samples VALUES(?1,?2,?3,1,'fixture','hot')",
                    )?
                    .execute(params![
                        epoch,
                        ns as i64,
                        (ns + 200_000_000) as i64
                    ])?;
                    db.prepare_cached("UPDATE video_segments SET end_ns=?2 WHERE id=?1")?
                        .execute(params![epoch, (ns + 200_000_000) as i64])?;
                    if enabled {
                        let context = Context {
                            app: Some("browser".into()),
                            window_id: Some(1),
                            window_title: if changing {
                                format!("Page {}", ns / (10 * SAMPLE_NS))
                            } else {
                                "Page".into()
                            },
                        };
                        let mut staged = detector.clone();
                        let mut o = staged.observe(ns, 1728, 1116, &pixels, context.clone())?;
                        if i % 300 == 0 && o.is_none() {
                            o = Some(Observation {
                                context,
                                change: ScreenChange::Unchanged,
                            });
                        }
                        if let Some(o) = o {
                            observe(&db, epoch, ns, &o)?;
                        }
                        detector = staged;
                    }
                }
                if enabled {
                    project(&db, 60)?;
                }
                timings.push(cpu() - begin);
                if enabled {
                    rows = 0;
                    for table in [
                        "timeline_chunks",
                        "chunk_tags",
                        "chunk_links",
                        "video_screen_events",
                        "timeline_video_backfill",
                        "timeline_screen_detector",
                    ] {
                        rows +=
                            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                                r.get::<_, i64>(0)
                            })?;
                    }
                    states = db.query_row(
                        "SELECT count(*) FROM timeline_chunks WHERE kind='screen'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )?;
                }
                db.execute_batch("ROLLBACK")?;
            }
            println!("SCREEN_COST changing={changing} baseline_cpu_s_per_hour={:.6} indexed_cpu_s_per_hour={:.6} extra_cpu_s_per_hour={:.6} rows_per_hour={rows} screen_states_per_hour={states}",(timings[0]+timings[3])/2.,(timings[1]+timings[2])/2.,(timings[1]+timings[2]-timings[0]-timings[3])/2.);
        }
        Ok(())
    }
}
