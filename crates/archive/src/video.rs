//! Durable, per-display encoded video on capture's monotonic clock. Each sample
//! is an independently committed pack blob; a segment is a bounded codec epoch,
//! not a loose movie that needs a final trailer after a crash.
use crate::{compact, hash, store, DAY_NS};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
#[path = "video_native.rs"]
pub mod native;

pub const SEGMENT_NS: u64 = 60_000_000_000;
pub const MAX_SAMPLE_BYTES: usize = 32 << 20;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Geometry {
    pub display: u32,
    pub x: f64,
    pub y: f64,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub hvcc: Vec<u8>,
    pub parameters: Vec<Vec<u8>>,
}
#[derive(Clone, Debug)]
pub struct Sample {
    pub host_ns: u64,
    pub end_ns: u64,
    pub key: bool,
    pub data: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct Segment {
    pub id: i64,
    pub session: i64,
    pub geometry: Geometry,
    pub format: Format,
    pub start_ns: u64,
    pub end_ns: u64,
    pub wall_ns: u64,
    pub fps: u32,
    pub tier: String,
    pub closed: bool,
}

pub fn schema() -> &'static str {
    "
CREATE TABLE IF NOT EXISTS video_segments(id INTEGER PRIMARY KEY,session INTEGER NOT NULL,display INTEGER NOT NULL,geometry TEXT NOT NULL,format TEXT NOT NULL,start_ns INTEGER NOT NULL,end_ns INTEGER NOT NULL,wall_ns INTEGER NOT NULL,fps INTEGER NOT NULL,tier TEXT NOT NULL,closed INTEGER NOT NULL DEFAULT 0,owner_pid INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS video_time ON video_segments(session,display,start_ns);
CREATE INDEX IF NOT EXISTS video_age ON video_segments(tier,closed,wall_ns);
CREATE TABLE IF NOT EXISTS video_samples(segment INTEGER NOT NULL,host_ns INTEGER NOT NULL,end_ns INTEGER NOT NULL,keyframe INTEGER NOT NULL,sha256 TEXT NOT NULL,pack TEXT NOT NULL,PRIMARY KEY(segment,host_ns)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS video_blob ON video_samples(sha256,pack);
CREATE TABLE IF NOT EXISTS video_gaps(id INTEGER PRIMARY KEY,session INTEGER NOT NULL,display INTEGER NOT NULL,host_ns INTEGER NOT NULL,reason TEXT NOT NULL);
"
}

pub fn key(sha: &str) -> String {
    format!("v/{sha}")
}
/// The recording's writes go through the store's writer (`live.rs`): a sample's blob is committed to the hot pack
/// first, on the caller, and its rows by a writer job that re-checks the blob under the index write lock, like the
/// audio and image writers. The screen callback waits for a sample's commit at most `live::COMMIT_WAIT`, and only while
/// the writer keeps up; an epoch's first row waits at most `BEGIN_WAIT`.
pub struct Writer {
    root: PathBuf,
    session: i64,
    queue: crate::live::Queue,
}
/// The longest `begin` waits for its epoch row (it needs the row's ID).
pub const BEGIN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
impl Writer {
    pub fn open(session: &Path) -> Result<Self> {
        let (root, name) = store::locate(session)?;
        let queue = crate::live::Queue::shared(&root)?;
        let db = store::open(&root)?;
        let session = store::ensure_session(&db, &name)?;
        Ok(Self { root, session, queue })
    }
    pub fn recover_previous(&self) -> Result<()> {
        let db = store::open(&self.root)?;
        let owners = db
            .prepare(
                "SELECT DISTINCT owner_pid FROM video_segments WHERE closed=0 AND session<>?1",
            )?
            .query_map([self.session], |r| r.get::<_, i32>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for pid in owners {
            if pid > 0
                && unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                let session = self.session;
                self.queue.call(move |db| Ok(db.execute("UPDATE video_segments SET closed=1 WHERE closed=0 AND owner_pid=?1 AND session<>?2",params![pid,session])?))?;
            }
        }
        Ok(())
    }
    pub fn begin(
        &self,
        geometry: &Geometry,
        format: &Format,
        host_ns: u64,
        wall_ns: u64,
        fps: u32,
    ) -> Result<i64> {
        ensure!(
            format.width > 0 && format.height > 0 && (1..=30).contains(&fps),
            "invalid video format"
        );
        let (session, display, geometry, format) = (self.session, geometry.display, serde_json::to_string(geometry)?, serde_json::to_string(format)?);
        // A begin its caller gave up on still commits later: then as a closed epoch, never an open one nobody appends to.
        let abandoned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gave_up = abandoned.clone();
        let id = self.queue.call_within(BEGIN_WAIT, move |db| {
            let closed = abandoned.load(std::sync::atomic::Ordering::Acquire);
            db.execute("INSERT INTO video_segments(session,display,geometry,format,start_ns,end_ns,wall_ns,fps,tier,owner_pid,closed) VALUES(?1,?2,?3,?4,?5,?5,?6,?7,'hot',?8,?9)",
                params![session, display, geometry, format, host_ns as i64, wall_ns as i64, fps, std::process::id(), closed])?;
            Ok(db.last_insert_rowid())
        });
        if id.is_err() {
            gave_up.store(true, std::sync::atomic::Ordering::Release);
        }
        id
    }
    pub fn append(&self, segment: i64, sample: &Sample) -> Result<()> {
        self.append_screen(segment, sample, None)
    }
    pub fn append_screen(
        &self,
        segment: i64,
        sample: &Sample,
        observation: Option<&crate::timeline::recording::Observation>,
    ) -> Result<()> {
        ensure!(
            sample.end_ns > sample.host_ns
                && !sample.data.is_empty()
                && sample.data.len() <= MAX_SAMPLE_BYTES,
            "invalid encoded video sample"
        );
        let sha = hash(&sample.data);
        let blob = key(&sha);
        store::ensure_blob(&self.root, store::HOT, &blob, &sample.data)?;
        let (root, session, data) = (self.root.clone(), self.session, std::sync::Arc::new(sample.data.clone()));
        let (host_ns, end_ns, keyframe) = (sample.host_ns, sample.end_ns, sample.key);
        let observation = observation.cloned();
        self.queue.write(move |tx| {
            // The collector may have removed an identical blob since it was written above.
            store::ensure_blob(&root, store::HOT, &blob, &data)?;
            append_rows(tx, segment, session, host_ns, end_ns, keyframe, &sha, observation.as_ref())
        })?;
        Ok(())
    }
    /// An idle SCK sample extends coverage, without another encode or blob.
    pub fn extend(&self, segment: i64, end_ns: u64) -> Result<()> {
        self.extend_screen(segment, end_ns, None)
    }
    pub fn extend_screen(
        &self,
        segment: i64,
        end_ns: u64,
        observation: Option<&crate::timeline::recording::Observation>,
    ) -> Result<()> {
        self.extend_screen_at(segment,end_ns,observation,None)
    }
    pub fn extend_screen_at(&self,segment:i64,end_ns:u64,observation:Option<&crate::timeline::recording::Observation>,host_ns:Option<u64>)->Result<()> {
        let (session, observation) = (self.session, observation.cloned());
        self.queue.write(move |tx| {
            tx.execute("UPDATE video_samples SET end_ns=max(end_ns,?2) WHERE segment=?1 AND host_ns=(SELECT max(host_ns) FROM video_samples WHERE segment=?1) AND EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND session=?3 AND closed=0)",params![segment,end_ns as i64,session])?;
            tx.execute("UPDATE video_segments SET end_ns=max(end_ns,?2) WHERE id=?1 AND session=?3 AND closed=0",params![segment,end_ns as i64,session])?;
            if let Some(o) = &observation {
                let fps: u32 = tx.query_row("SELECT fps FROM video_segments WHERE id=?1", [segment], |r| r.get(0))?;
                crate::timeline::recording::observe(tx, segment, host_ns.unwrap_or_else(||end_ns.saturating_sub(1_000_000_000 / fps as u64)), o)?;
            }
            Ok(())
        })?;
        Ok(())
    }
    pub fn close(&self, segment: i64) -> Result<()> {
        self.close_at(segment,i64::MAX as u64)
    }
    /// Rotation clips the outgoing jitter tail at the incoming epoch's first frame.
    pub fn close_at(&self,segment:i64,end_ns:u64)->Result<()> {
        let session = self.session;
        self.queue.write(move |tx| {
            tx.execute("UPDATE video_samples SET end_ns=min(end_ns,?3) WHERE segment=?1 AND EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND session=?2 AND closed=0)",params![segment,session,end_ns as i64])?;
            tx.execute("UPDATE video_segments SET end_ns=min(end_ns,?3) WHERE id=?1 AND session=?2 AND closed=0",params![segment,session,end_ns as i64])?;
            tx.execute("UPDATE video_segments SET closed=1 WHERE id=?1 AND session=?2", params![segment, session])?;
            let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM video_screen_events WHERE segment=?1)", [segment], |r| r.get(0))?;
            if known {
                crate::timeline::recording::project(tx, segment)?;
            }
            Ok(())
        })?;
        Ok(())
    }
    pub fn gap(&self, display: u32, host_ns: u64, reason: &str) -> Result<()> {
        let (session, reason) = (self.session, reason.to_owned());
        self.queue.write(move |tx| {
            tx.execute("UPDATE video_samples SET end_ns=min(end_ns,?3) WHERE segment IN (SELECT id FROM video_segments WHERE session=?1 AND display=?2 AND closed=0)",params![session,display,host_ns as i64])?;
            tx.execute("UPDATE video_segments SET end_ns=min(end_ns,?3) WHERE session=?1 AND display=?2 AND closed=0",params![session,display,host_ns as i64])?;
            let ids:Vec<i64>=tx.prepare("SELECT id FROM video_segments s WHERE session=?1 AND display=?2 AND EXISTS(SELECT 1 FROM video_screen_events WHERE segment=s.id) ORDER BY start_ns DESC LIMIT 1")?.query_map(params![session,display],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for id in ids {
                crate::timeline::recording::project(tx, id)?;
            }
            crate::timeline::recording::stop(tx, session, display)?;
            tx.execute("INSERT INTO video_gaps(session,display,host_ns,reason) VALUES(?1,?2,?3,?4)", params![session, display, host_ns as i64, reason])?;
            Ok(())
        })?;
        Ok(())
    }
}
/// A sample's rows, in the writer's transaction: the epoch must be open and hot, the sample must follow the last one,
/// and an epoch begins with a keyframe.
#[allow(clippy::too_many_arguments)]
fn append_rows(tx: &rusqlite::Connection, segment: i64, session: i64, host_ns: u64, end_ns: u64, keyframe: bool, sha: &str, observation: Option<&crate::timeline::recording::Observation>) -> Result<()> {
    {
        let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND session=?2 AND closed=0 AND tier='hot' AND (SELECT max(host_ns) FROM video_samples WHERE segment=?1)<?3) OR EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND session=?2 AND closed=0 AND tier='hot' AND NOT EXISTS(SELECT 1 FROM video_samples WHERE segment=?1))", params![segment,session,host_ns as i64], |r| r.get(0))?;
        ensure!(valid, "video segment closed or sample regressed");
        let empty: bool = tx.query_row("SELECT NOT EXISTS(SELECT 1 FROM video_samples WHERE segment=?1)", [segment], |r| r.get(0))?;
        ensure!(!empty || keyframe, "a video epoch must begin with a keyframe");
        #[cfg(test)]
        {
            let mine = BEFORE_VIDEO_WRITE.lock().unwrap().take_if(|(path, _)| tx.path().is_some_and(|p| std::path::Path::new(p) == path.as_path()));
            if let Some((_, hook)) = mine {
                hook();
            }
        }
        tx.execute("UPDATE video_samples SET end_ns=min(end_ns,?2) WHERE segment=?1 AND host_ns=(SELECT max(host_ns) FROM video_samples WHERE segment=?1)", params![segment,host_ns as i64])?;
        tx.execute("INSERT INTO video_samples VALUES(?1,?2,?3,?4,?5,'hot')", params![segment, host_ns as i64, end_ns as i64, keyframe, sha])?;
        tx.execute("UPDATE video_segments SET end_ns=?2 WHERE id=?1", params![segment, end_ns as i64])?;
        if let Some(o) = observation {
            crate::timeline::recording::observe(tx, segment, host_ns, o)?;
        }
    }
    Ok(())
}
pub(crate) fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Segment> {
    let parse = |col| -> rusqlite::Result<String> { r.get(col) };
    let convert =
        |e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e));
    Ok(Segment {
        id: r.get(0)?,
        session: r.get(1)?,
        geometry: serde_json::from_str(&parse(2)?).map_err(convert)?,
        format: serde_json::from_str(&parse(3)?).map_err(convert)?,
        start_ns: r.get::<_, i64>(4)? as u64,
        end_ns: r.get::<_, i64>(5)? as u64,
        wall_ns: r.get::<_, i64>(6)? as u64,
        fps: r.get(7)?,
        tier: r.get(8)?,
        closed: r.get(9)?,
    })
}
pub fn segments(root: &Path, session: i64) -> Result<Vec<Segment>> {
    range(root, session, 0, i64::MAX as u64)
}
pub fn range(root: &Path, session: i64, from: u64, to: u64) -> Result<Vec<Segment>> {
    let db = store::read_index(root)?;
    let mut s=db.prepare("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE session=?1 AND end_ns>?2 AND start_ns<=?3 ORDER BY display,start_ns,id")?;
    let rows = s
        .query_map(
            params![session, from as i64, to.min(i64::MAX as u64) as i64],
            row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
pub fn samples(root: &Path, segment: i64, through_ns: u64) -> Result<Vec<Sample>> {
    let db = store::read_index(root)?;
    let mut stmt=db.prepare("SELECT host_ns,end_ns,keyframe,sha256,pack FROM video_samples WHERE segment=?1 AND host_ns<=?2 ORDER BY host_ns")?;
    let rows = stmt
        .query_map(params![segment, through_ns as i64], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, bool>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut packs = std::collections::BTreeMap::new();
    let mut out = Vec::new();
    let mut bytes = 0;
    for (host_ns, end_ns, key, sha, pack) in rows {
        // Retrieval never initializes/schema-writes a content pack. Keep one
        // read-only connection per pack for the entire bounded codec epoch.
        if !packs.contains_key(&pack) {
            let reader = rusqlite::Connection::open_with_flags(store::pack_path(root, &pack)?, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            reader.busy_timeout(store::BUSY)?;
            packs.insert(pack.clone(), reader);
        }
        let data: Vec<u8> = packs.get(&pack).unwrap()
            .query_row("SELECT data FROM blobs WHERE key=?1", [self::key(&sha)], |r| r.get(0))
            .optional()?.with_context(|| format!("archive blob {pack}/{} missing", self::key(&sha)))?;
        ensure!(hash(&data) == sha, "archive blob {pack}/{} checksum mismatch", self::key(&sha));
        bytes += data.len();
        ensure!(
            bytes <= 128 << 20,
            "video segment exceeds decode memory bound"
        );
        out.push(Sample {
            host_ns,
            end_ns,
            key,
            data,
        });
    }
    Ok(out)
}
/// Select from committed coverage only; idle coverage is explicit, never invented
/// across a lock/sleep/error gap. `now` does not age out earlier recorded footage.
pub fn at(session: &Path, display: u32, host_ns: u64) -> Result<Option<(Segment, Vec<Sample>)>> {
    let (root, name) = store::locate(session)?;
    let _guard = compact::reader_lock(&root)?;
    let db = store::read_index(&root)?;
    let id = store::find_session(&db, &name)?.context("video session missing")?;
    let seg=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE session=?1 AND display=?2 AND start_ns<=?3 AND end_ns>?3 ORDER BY start_ns DESC,id DESC LIMIT 1",params![id,display,host_ns as i64],row).optional()?;
    match seg {
        Some(s) => {
            let frames = samples(&root, s.id, host_ns)?;
            Ok(frames.last().is_some_and(|f| host_ns < f.end_ns).then_some((s, frames)))
        }
        None => Ok(None),
    }
}
/// Resolve a durable representative against the current hot/cold codec epoch.
/// Re-encoding may replace the exact original timestamp; select its covering sample.
pub fn representative_png(root:&Path,session:i64,id:i64,host:u64,path:&Path,more:&mut dyn FnMut()->bool)->Result<()> {
    let _reader=compact::reader_lock(root)?;
    let db=store::read_index(root)?;
    let segment=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE id=?1 AND session=?2",params![id,session],row)?;
    let frames=samples(root,id,host)?;
    let last=frames.last().context("representative video frame missing")?;
    ensure!(host<last.end_ns,"representative video frame outside coverage");
    let image=if segment.format.parameters.is_empty() {
        image::load_from_memory(&last.data)?.to_rgb8()
    } else {
        #[cfg(target_os="macos")]
        {
            let mut decoder=native::Decoder::new(&segment.format)?;
            let mut rgb=vec![];
            for (i,sample) in frames.iter().enumerate() {
                ensure!(more(),"embedding video preparation cancelled");
                if i+1==frames.len() {rgb=decoder.decode(sample)?;} else {decoder.advance(sample)?;}
            }
            image::RgbImage::from_raw(segment.format.width,segment.format.height,rgb).context("decoded video geometry")?
        }
        #[cfg(not(target_os="macos"))]
        anyhow::bail!("HEVC representative requires macOS");
    };
    ensure!(more(),"embedding video preparation cancelled");
    image.save_with_format(path,image::ImageFormat::Png)?;
    Ok(())
}
/// Close abandoned epochs at startup; their committed samples remain reconstructible.
pub fn recover(root: &Path) -> Result<()> {
    let db = store::open(root)?;
    store::written(&db, |db| Ok(db.execute("UPDATE video_segments SET closed=1 WHERE closed=0 AND session IN (SELECT id FROM sessions WHERE complete_wall_ns IS NOT NULL)",[])?))?;
    Ok(())
}

/// One closed <=60 s epoch at a time, behind the same lifetime and power gate as
/// audio. Verify every new sample decodes before atomically replacing hot refs.
#[cfg(target_os = "macos")]
pub fn compact_while(root: &Path, now: u64, more: &mut dyn FnMut() -> bool) -> Result<usize> {
    let Some(_owner) = compact::owner(root)? else {
        return Ok(0);
    };
    let queue = crate::live::Queue::shared(root)?;
    let db = store::open(root)?;
    let mut stmt=db.prepare("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE tier='hot' AND closed=1 AND wall_ns+(end_ns-start_ns)<=?1 ORDER BY wall_ns LIMIT 4")?;
    let epochs = stmt
        .query_map([now.saturating_sub(DAY_NS) as i64], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut count = 0;
    for candidate in epochs {
        if !more() {
            break;
        }
        let reader = compact::reader_lock(root)?;
        let Some(seg)=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE id=?1 AND tier='hot' AND closed=1",[candidate.id],row).optional()? else {continue};
        let old = samples(root, seg.id, u64::MAX >> 1)?;
        if old.is_empty() {
            continue;
        }
        let mut decoder = native::Decoder::new(&seg.format)?;
        let mut encoder = native::Encoder::new(seg.format.width, seg.format.height, 1, 0.5)?;
        let mut cold = vec![];
        let mut format = None;
        let mut next = seg.start_ns;
        for sample in &old {
            if !more() {
                return Ok(count);
            }
            if sample.host_ns >= next || cold.last().is_none_or(|s: &Sample|sample.host_ns>s.end_ns) {
                let rgb = decoder.decode(sample)?;
                let (f, mut s) = encoder.encode(&rgb, sample.host_ns, sample.end_ns)?;
                if let Some(prev) = cold.last_mut() {
                    let prev: &mut Sample = prev;
                    prev.end_ns = prev.end_ns.min(s.host_ns);
                }
                s.end_ns = sample.end_ns;
                cold.push(s);
                format = Some(f);
                next = sample.host_ns + 1_000_000_000;
            } else {
                decoder.advance(sample)?;
                cold.last_mut().unwrap().end_ns = sample.end_ns;
            }
        }
        let format = format.context("empty archival epoch")?;
        let mut check = native::Decoder::new(&format)?;
        for s in &cold {
            if !more() {return Ok(count);}
            let rgb = check.decode(s)?;
            ensure!(
                rgb.len() == (format.width as usize * format.height as usize * 3),
                "archival video verification failed"
            );
        }
        if !more() {return Ok(count);}
        let pack = store::month(seg.wall_ns);
        let items: Vec<_> = cold
            .iter()
            .map(|s| (key(&hash(&s.data)), s.data.as_slice()))
            .collect();
        store::put(root, &pack, &items)?;
        drop(reader);
        if !more() {return Ok(count);}
        // The reference swap is one writer job. It runs only while no reader holds the archive (a pinned command
        // keeps its frames hot), re-checks the epoch, and queues the hot blobs for collection.
        let (r, id, format) = (root.to_path_buf(), seg.id, serde_json::to_string(&format)?);
        let swapped = queue.background(move |tx| {
            let Some(_lock) = compact::exclusive_lock(&r)? else { return Ok(false) };
            let unchanged: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND tier='hot' AND closed=1)", [id], |r| r.get(0))?;
            if !unchanged {
                return Ok(false);
            }
            let refs = tx
                .prepare("SELECT pack,sha256 FROM video_samples WHERE segment=?1")?
                .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (p, sha) in refs {
                store::queue_gc(tx, &p, &key(&sha))?;
            }
            tx.execute("DELETE FROM video_samples WHERE segment=?1", [id])?;
            for s in &cold {
                let sha = hash(&s.data);
                ensure!(store::has(&r, &pack, &key(&sha))?, "archival video blob missing");
                tx.execute("INSERT INTO video_samples VALUES(?1,?2,?3,?4,?5,?6)", params![id, s.host_ns as i64, s.end_ns as i64, s.key, sha, pack])?;
            }
            tx.execute("UPDATE video_segments SET tier='archive',fps=1,format=?2 WHERE id=?1", params![id, format])?;
            Ok(true)
        })?;
        if swapped {
            count += 1;
        }
    }
    Ok(count)
}

/// Runs inside the next sample's writer transaction, before its rows (tests).
#[cfg(test)]
static BEFORE_VIDEO_WRITE: std::sync::Mutex<Option<(PathBuf, Box<dyn FnOnce() + Send>)>> = std::sync::Mutex::new(None);

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path) -> (Writer, i64) {
        let writer = Writer::open(&root.join("session-video")).unwrap();
        let geometry = Geometry {
            display: 1,
            x: 0.,
            y: 0.,
            width: 64,
            height: 32,
        };
        let format = Format {
            width: 64,
            height: 32,
            hvcc: vec![],
            parameters: vec![],
        };
        let segment = writer
            .begin(&geometry, &format, 1_000_000_000, 1, 5)
            .unwrap();
        (writer, segment)
    }
    #[test]
    fn video_reserves_the_writer_before_its_snapshot_and_retrieval_remains_read_only() {
        let root = tempfile::tempdir().unwrap();
        let (writer, id) = fixture(root.path());
        let mut stale = store::open(root.path()).unwrap();
        let other = store::open(root.path()).unwrap();
        // Reproduce the historical read -> other writer commit -> write race.
        let tx = stale.transaction().unwrap();
        tx.query_row("SELECT closed FROM video_segments WHERE id=?1", [id], |r| r.get::<_, bool>(0)).unwrap();
        other.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('audio-race',0)", []).unwrap();
        let error = tx.execute("UPDATE video_segments SET end_ns=end_ns WHERE id=?1", [id]).unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.extended_code == 517), "expected SQLITE_BUSY_SNAPSHOT: {error}");
        tx.rollback().unwrap();

        let path = root.path().join(store::INDEX);
        let index = path.canonicalize().unwrap();
        *BEFORE_VIDEO_WRITE.lock().unwrap() = Some((index, Box::new(move || {
            let rival = rusqlite::Connection::open(path).unwrap();
            rival.busy_timeout(std::time::Duration::ZERO).unwrap();
            let error = rival.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('competing-evidence',0)", []).unwrap_err();
            assert!(matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DatabaseBusy));
        })));
        writer.append(id, &Sample { host_ns:1_000_000_000, end_ns:1_200_000_000, key:true, data:vec![1,2,3] }).unwrap();
        other.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('competing-evidence',0)", []).unwrap();
        writer.append(id, &Sample { host_ns:1_200_000_000, end_ns:1_400_000_000, key:false, data:vec![4,5,6] }).unwrap();
        let pack = rusqlite::Connection::open(store::pack_path(root.path(), store::HOT).unwrap()).unwrap();
        pack.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (segment, samples) = at(&root.path().join("session-video"), 1, 1_300_000_000).unwrap().unwrap();
        assert_eq!(segment.id,id);
        assert_eq!(samples.last().unwrap().data,[4,5,6], "screenshot retrieval beside an active pack writer");
        pack.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn committed_samples_survive_abrupt_writer_end_and_idle_is_one_blob() {
        let root = tempfile::tempdir().unwrap();
        let (writer, id) = fixture(root.path());
        writer
            .append(
                id,
                &Sample {
                    host_ns: 1_000_000_000,
                    end_ns: 1_200_000_000,
                    key: true,
                    data: vec![1, 2, 3],
                },
            )
            .unwrap();
        writer.extend(id, 10_200_000_000).unwrap();
        drop(writer);
        let (segment, samples) = at(&root.path().join("session-video"), 1, 10_000_000_000)
            .unwrap()
            .unwrap();
        assert!(!segment.closed);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].end_ns, 10_200_000_000);
        assert!(at(&root.path().join("session-video"), 1, 10_200_000_000)
            .unwrap()
            .is_none());
        let db = store::open(root.path()).unwrap();
        assert!(store::referenced(&db, "hot", &key(&hash(&[1, 2, 3]))).unwrap());
        store::ensure_blob(root.path(), "hot", "v/unpublished", &[9]).unwrap();
        assert!(!store::referenced(&db, "hot", "v/unpublished").unwrap());
    }
    #[test]
    #[ignore = "only invoked by the crash-safety parent with an isolated root"]
    fn crash_writer_child() {
        let root = std::env::var_os("HEIMDALL_VIDEO_CRASH_ROOT").expect("crash test root");
        let (writer, id) = fixture(Path::new(&root));
        writer
            .append(
                id,
                &Sample {
                    host_ns: 1_000_000_000,
                    end_ns: 1_200_000_000,
                    key: true,
                    data: vec![4, 5, 6],
                },
            )
            .unwrap();
        unsafe { libc::raise(libc::SIGKILL) };
    }
    #[test]
    fn killed_writer_preserves_committed_tail_and_recovery_never_closes_live_owner() {
        use std::os::unix::process::ExitStatusExt;
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "video::tests::crash_writer_child"])
            .env("HEIMDALL_VIDEO_CRASH_ROOT", root.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        let session = root.path().join("session-video");
        let (old, samples) = at(&session, 1, 1_000_000_000).unwrap().unwrap();
        assert_eq!(samples[0].data, [4, 5, 6]);
        assert!(!old.closed);
        let next = Writer::open(&root.path().join("session-next")).unwrap();
        next.recover_previous().unwrap();
        assert!(at(&session, 1, 1_000_000_000).unwrap().unwrap().0.closed);
        let (_, live) = fixture(root.path());
        next.recover_previous().unwrap();
        let db = store::open(root.path()).unwrap();
        assert!(!db
            .query_row(
                "SELECT closed FROM video_segments WHERE id=?1",
                [live],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn hardware_roundtrip_and_cold_tier_are_reconstructible() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session-codec");
        let writer = Writer::open(&session).unwrap();
        let mut encoder = native::Encoder::new(64, 32, 5, 0.8).unwrap();
        let rgb: Vec<_> = (0..64 * 32)
            .flat_map(|i| [if i % 2 == 0 { 20 } else { 220 }; 3])
            .collect();
        let (format, sample) = encoder.encode(&rgb, 1_000_000_000, 1_200_000_000).unwrap();
        assert!(sample.key);
        assert!(!format.hvcc.is_empty());
        let id = writer
            .begin(
                &Geometry {
                    display: 1,
                    x: 0.,
                    y: 0.,
                    width: 64,
                    height: 32,
                },
                &format,
                sample.host_ns,
                1,
                5,
            )
            .unwrap();
        writer.append(id, &sample).unwrap();
        for i in 1..10 {
            let (_, s) = encoder
                .encode(
                    &rgb,
                    1_000_000_000 + i * 200_000_000,
                    1_200_000_000 + i * 200_000_000,
                )
                .unwrap();
            writer.append(id, &s).unwrap();
        }
        writer.close(id).unwrap();
        let hot = at(&session, 1, 2_800_000_000).unwrap().unwrap();
        let mut decoder = native::Decoder::new(&hot.0.format).unwrap();
        let mut decoded = vec![];
        for s in &hot.1 {
            decoded = decoder.decode(s).unwrap();
        }
        assert_eq!(decoded.len(), rgb.len());
        let mean_error = decoded
            .iter()
            .zip(&rgb)
            .map(|(a, b)| (*a as f64 - *b as f64).abs())
            .sum::<f64>()
            / rgb.len() as f64;
        assert!(mean_error < 20., "codec error {mean_error}");
        assert_eq!(
            compact_while(root.path(), DAY_NS + 5_000_000_000, &mut || false).unwrap(),
            0
        );
        assert_eq!(
            compact_while(root.path(), DAY_NS + 5_000_000_000, &mut || true).unwrap(),
            1
        );
        let cold = at(&session, 1, 2_800_000_000).unwrap().unwrap();
        assert_eq!(cold.0.tier, "archive");
        assert_eq!(cold.0.fps, 1);
        assert_eq!(cold.1.len(), 2);
        let mut decoder = native::Decoder::new(&cold.0.format).unwrap();
        for s in &cold.1 {
            assert_eq!(decoder.decode(s).unwrap().len(), rgb.len());
        }
        compact::collect_now(root.path()).unwrap();
        assert!(at(&session, 1, 2_800_000_000).unwrap().is_some());
    }
}
