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
        let data = sample_blob(&mut packs, root, &pack, &sha)?;
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
    Frames::default().png(root,session,id,host,path,more)
}
/// Bytes of one committed sample from its pack, checked against its hash. `packs` keeps one read-only connection per
/// pack for the caller's bounded read.
fn sample_blob(packs:&mut std::collections::BTreeMap<String,rusqlite::Connection>,root:&Path,pack:&str,sha:&str)->Result<Vec<u8>> {
    if !packs.contains_key(pack) {
        let reader=rusqlite::Connection::open_with_flags(store::pack_path(root,pack)?,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        reader.busy_timeout(store::BUSY)?;
        packs.insert(pack.to_owned(),reader);
    }
    let data:Vec<u8>=packs[pack].query_row("SELECT data FROM blobs WHERE key=?1",[key(sha)],|r|r.get(0))
        .optional()?.with_context(||format!("archive blob {pack}/{} missing",key(sha)))?;
    ensure!(hash(&data)==sha,"archive blob {pack}/{} checksum mismatch",key(sha));
    Ok(data)
}
/// Representative frames for one embedding job (a group of screen states, or one moment's screens). An HEVC frame
/// decodes from its epoch's first sample; asked in rising time order, one decoder runs forward through the epoch once
/// instead of starting again for every frame. Sample of the installed app (13 Nov 14:50, 20 s): 320 of the indexing
/// thread's 428 busy samples waited on VideoToolbox inside `representative_png`, and 9 in 10 of those decoded the
/// frames before the one wanted; a screen state sat about 62 samples into its epoch, and a moment carried about 9.5
/// screen frames, each decoded from the epoch's start. Pixels are those of a fresh decode: the decoder sees the same
/// samples in the same order, and resumes only while the epoch still holds the very samples it decoded.
#[derive(Default)]
pub struct Frames {
    #[cfg(target_os="macos")]
    open: Option<OpenEpoch>,
    /// Compressed samples read and decoded so far.
    pub decoded: usize,
}
#[cfg(target_os="macos")]
struct OpenEpoch {
    segment: i64,
    parameters: Vec<Vec<u8>>,
    decoder: native::Decoder,
    /// Hashes of the samples decoded, in order: the epoch's first `shas.len()` samples.
    shas: Vec<String>,
    bytes: usize,
    /// Pixels of the last sample decoded, when it was a wanted frame.
    rgb: Option<Vec<u8>>,
}
impl Frames {
    /// Writes the representative frame of epoch `id` at `host` to `path` as PNG.
    pub fn png(&mut self,root:&Path,session:i64,id:i64,host:u64,path:&Path,more:&mut dyn FnMut()->bool)->Result<()> {
        let _reader=compact::reader_lock(root)?;
        let db=store::read_index(root)?;
        let segment=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE id=?1 AND session=?2",params![id,session],row)?;
        let rows:Vec<(u64,u64,bool,String,String)>=db.prepare("SELECT host_ns,end_ns,keyframe,sha256,pack FROM video_samples WHERE segment=?1 AND host_ns<=?2 ORDER BY host_ns")?
            .query_map(params![id,host as i64],|r|Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64,r.get(2)?,r.get(3)?,r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let last=rows.last().context("representative video frame missing")?;
        ensure!(host<last.1,"representative video frame outside coverage");
        let mut packs=std::collections::BTreeMap::new();
        let image=if segment.format.parameters.is_empty() {
            let data=sample_blob(&mut packs,root,&last.4,&last.3)?;
            self.decoded+=1;
            image::load_from_memory(&data)?.to_rgb8()
        } else {
            #[cfg(target_os="macos")]
            {
                // Resume only an epoch whose decoded samples are still its first ones (re-encoding changes them).
                let resume=self.open.take().filter(|o| o.segment==id && o.parameters==segment.format.parameters
                    && o.shas.len()<=rows.len() && o.shas.iter().zip(&rows).all(|(sha,r)|*sha==r.3)
                    && (o.shas.len()<rows.len() || o.rgb.is_some()));
                let mut open=match resume {
                    Some(open)=>open,
                    None=>OpenEpoch{segment:id,parameters:segment.format.parameters.clone(),decoder:native::Decoder::new(&segment.format)?,shas:vec![],bytes:0,rgb:None},
                };
                for (i,(host_ns,end_ns,key,sha,pack)) in rows.iter().enumerate().skip(open.shas.len()) {
                    ensure!(more(),"embedding video preparation cancelled");
                    let data=sample_blob(&mut packs,root,pack,sha)?;
                    open.bytes+=data.len();
                    ensure!(open.bytes<=128<<20,"video segment exceeds decode memory bound");
                    let sample=Sample{host_ns:*host_ns,end_ns:*end_ns,key:*key,data};
                    self.decoded+=1;
                    open.rgb=None;
                    if i+1==rows.len() {open.rgb=Some(open.decoder.decode(&sample)?);} else {open.decoder.advance(&sample)?;}
                    open.shas.push(sha.clone());
                }
                let rgb=open.rgb.clone().context("decoded video frame")?;
                self.open=Some(open);
                image::RgbImage::from_raw(segment.format.width,segment.format.height,rgb).context("decoded video geometry")?
            }
            #[cfg(not(target_os="macos"))]
            anyhow::bail!("HEVC representative requires macOS");
        };
        ensure!(more(),"embedding video preparation cancelled");
        image.save_with_format(path,image::ImageFormat::Png)?;
        Ok(())
    }
}
/// Close abandoned epochs at startup; their committed samples remain reconstructible.
pub fn recover(root: &Path) -> Result<()> {
    let db = store::open(root)?;
    store::written(&db, |db| Ok(db.execute("UPDATE video_segments SET closed=1 WHERE closed=0 AND session IN (SELECT id FROM sessions WHERE complete_wall_ns IS NOT NULL)",[])?))?;
    Ok(())
}

/// Epochs one compaction pass tiers at most before metadata packing and the caller's gate get a turn. The gate is
/// asked between frames as well, so a larger budget never delays a stop; 4 left the pass loop's own overhead
/// (audio scan, chunk packing, timeline catch-up) between every four epochs of a 5,513-epoch backlog (12 Nov).
pub const PASS_EPOCHS: usize = 16;
/// Closed empty epochs one writer job retires at most.
pub const RETIRE_EPOCHS: usize = 512;
/// An epoch whose tiering failed is not tried again in this process for this long; the next ones go on meanwhile.
pub const FAILED_RETRY: std::time::Duration = std::time::Duration::from_secs(3600);
/// An epoch whose tiering failed this many times (each `FAILED_RETRY` apart, across restarts: `video_tier_failures`)
/// is parked: it keeps its hot footage, is reported once with its reason and is not tried again (review of the night
/// pass, 13 Nov, non-blocker 5: a bad epoch was decoded again every hour and its error came back each pass).
/// `DELETE FROM video_tier_failures` lets parked epochs try again.
pub const PARK_AFTER: i64 = 3;
/// What one tiering call did.
#[derive(Debug, Default)]
pub struct Tiering {
    /// Epochs re-encoded into their month pack.
    pub tiered: usize,
    /// Closed epochs without a sample, older than a day, marked `archive` (nothing to re-encode).
    pub retired: usize,
    /// One line per epoch that failed; it keeps its hot footage and is retried after `FAILED_RETRY`, until it is
    /// parked (`PARK_AFTER`), which its last line says.
    pub errors: Vec<String>,
    /// Epochs parked by this call (`PARK_AFTER`).
    pub parked: usize,
}
fn failed() -> &'static std::sync::Mutex<std::collections::HashMap<(PathBuf, i64), std::time::Instant>> {
    static FAILED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<(PathBuf, i64), std::time::Instant>>> = std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}
/// Marks closed hot epochs without a sample whose span ended before `cutoff` as `archive`, `RETIRE_EPOCHS` per writer
/// job. An epoch whose first row met a busy archive is left closed and empty (`docs/screen.md`); before 13 Nov tiering
/// skipped such rows without retiring them, and from 2 Nov 15:51 four of them filled every slot of the oldest-first
/// selection, so no epoch was tiered for ten days and hot.sqlite grew to 90 GB (disk audit, 12 Nov).
pub fn retire_empty(queue: &crate::live::Queue, cutoff: u64) -> Result<usize> {
    let mut retired = 0;
    loop {
        let n = queue.background(move |tx| {
            Ok(tx.execute(
                "UPDATE video_segments SET tier='archive' WHERE id IN (SELECT id FROM video_segments s WHERE tier='hot' AND closed=1 AND wall_ns+(end_ns-start_ns)<=?1 AND NOT EXISTS(SELECT 1 FROM video_samples WHERE segment=s.id) LIMIT ?2)",
                params![cutoff as i64, RETIRE_EPOCHS as i64],
            )?)
        })?;
        retired += n;
        if n < RETIRE_EPOCHS {
            return Ok(retired);
        }
    }
}
/// How long tiering waits for the archive's readers to let go, at an epoch's start and again at its swap, before it
/// leaves the epoch hot for the next pass. A reader holds the lifetime lock for one bounded step; the search indexer
/// took it back within a third of a second all day on 13 Nov, and a pass that gave up at its first refused probe
/// tiered nothing from 10:22 to 20:03 while the backlog grew from 117 to 229 epochs.
pub const HELD_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// How often a waiting pass probes the lock again, and how often it asks the caller's gate meanwhile (the app's asks
/// the power source and idle time).
const HELD_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const HELD_ASK: std::time::Duration = std::time::Duration::from_secs(1);
/// Waits up to `wait` until no reader holds the archive (true), or `more()` refuses. The lock itself is not kept:
/// holding it would make every new reader wait for this pass.
fn readers_gone(root: &Path, wait: std::time::Duration, more: &mut dyn FnMut() -> bool) -> Result<bool> {
    let started = std::time::Instant::now();
    let mut asked = started;
    loop {
        if compact::exclusive_lock(root)?.is_some() {
            return Ok(true);
        }
        if started.elapsed() >= wait {
            return Ok(false);
        }
        if asked.elapsed() >= HELD_ASK {
            if !more() {
                return Ok(false);
            }
            asked = std::time::Instant::now();
        }
        std::thread::sleep(HELD_POLL);
    }
}
/// Tiers closed epochs older than a day, oldest first, up to `max` per call, behind the same lifetime and power gate as
/// audio. Every new sample is verified to decode before the hot references are replaced in one writer job. Empty
/// epochs are retired first; an epoch that fails, or that a pinned command keeps hot, is passed over for the next one,
/// so no epoch can hold the queue. After each tiered epoch its hot blobs are collected and the hot pack gives back the
/// pages, as the audio and image paths do.
#[cfg(target_os = "macos")]
pub fn tier_while(root: &Path, now: u64, max: usize, more: &mut dyn FnMut() -> bool) -> Result<Tiering> {
    tier_waiting(root, now, max, HELD_WAIT, more)
}
#[cfg(target_os = "macos")]
fn tier_waiting(root: &Path, now: u64, max: usize, wait: std::time::Duration, more: &mut dyn FnMut() -> bool) -> Result<Tiering> {
    let mut result = Tiering::default();
    let Some(_owner) = compact::owner(root)? else {
        return Ok(result);
    };
    let queue = crate::live::Queue::shared(root)?;
    let db = store::open(root)?;
    let cutoff = now.saturating_sub(DAY_NS);
    result.retired = retire_empty(&queue, cutoff)?;
    let key_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    failed().lock().unwrap().retain(|_, at| at.elapsed() < FAILED_RETRY);
    let (mut after_wall, mut after_id) = (i64::MIN, i64::MIN);
    let (mut seen, mut stopped, mut kept_hot) = (0usize, false, 0usize);
    let mut holder = None;
    while result.tiered < max {
        if !more() {
            stopped = true;
            break;
        }
        let Some(candidate) = db
            .prepare_cached("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments s WHERE tier='hot' AND closed=1 AND wall_ns+(end_ns-start_ns)<=?1 AND (wall_ns>?2 OR (wall_ns=?2 AND id>?3)) AND NOT EXISTS(SELECT 1 FROM video_tier_failures f WHERE f.segment=s.id AND f.failures>=?4) ORDER BY wall_ns,id LIMIT 1")?
            .query_row(params![cutoff as i64, after_wall, after_id, PARK_AFTER], row)
            .optional()?
        else {
            break;
        };
        (after_wall, after_id) = (candidate.wall_ns as i64, candidate.id);
        seen += 1;
        if failed().lock().unwrap().contains_key(&(key_root.clone(), candidate.id)) {
            continue;
        }
        match tier_epoch(root, &db, &queue, candidate.id, wait, more) {
            Ok(Epoch::Tiered) => result.tiered += 1,
            Ok(Epoch::Retired) => result.retired += 1,
            Ok(Epoch::KeptHot) => kept_hot += 1,
            // A reader held the archive through the whole wait (a pinned screen command, a clip export): every later
            // epoch would be decoded and re-encoded only to find the same lock at its swap. The next pass tries again
            // (review, 13 Nov).
            Ok(Epoch::Held(readers)) => {
                kept_hot += 1;
                holder = Some(readers);
                break;
            }
            Ok(Epoch::Stopped) => {
                stopped = true;
                break;
            }
            Err(e) => {
                let text = format!("{e:#}");
                if compact::interrupted(&text) {
                    return Err(e);
                }
                failed().lock().unwrap().insert((key_root.clone(), candidate.id), std::time::Instant::now());
                let (id, reason) = (candidate.id, text.clone());
                let failures = queue.background(move |tx| {
                    tx.execute(
                        "INSERT INTO video_tier_failures VALUES(?1,1,?2,?3) ON CONFLICT(segment) DO UPDATE SET failures=failures+1,reason=?2,wall_ns=?3",
                        params![id, reason, crate::wall_ns() as i64],
                    )?;
                    Ok(tx.query_row("SELECT failures FROM video_tier_failures WHERE segment=?1", [id], |r| r.get::<_, i64>(0))?)
                });
                match failures {
                    Ok(n) if n >= PARK_AFTER => {
                        result.parked += 1;
                        result.errors.push(format!("video epoch {id}: {text} (failed {n} times; parked with its hot footage, not tried again)"));
                    }
                    Ok(_) => result.errors.push(format!("video epoch {id}: {text}")),
                    Err(e) => result.errors.push(format!("video epoch {id}: {text} (its failure count was not kept: {e:#})")),
                }
            }
        }
    }
    if result.tiered == 0 && seen > 0 && !stopped {
        let eligible: i64 = db.query_row("SELECT count(*) FROM video_segments WHERE tier='hot' AND closed=1 AND wall_ns+(end_ns-start_ns)<=?1", [cutoff as i64], |r| r.get(0))?;
        report_stall(&format!(
            "Archive video tiering tiered none of {eligible} epochs older than a day ({} failed, {kept_hot} kept hot by a reader or a pinned command, {} waiting to retry after a failure){}",
            result.errors.len(),
            failed().lock().unwrap().keys().filter(|(r, _)| *r == key_root).count(),
            holder.map(|h| format!("; the archive was held by {h}")).unwrap_or_default(),
        ));
    }
    Ok(result)
}
/// Logs a stall at most once an hour per process: passes with nothing to do run every five minutes.
fn report_stall(line: &str) {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap();
    if last.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3600)) {
        eprintln!("{line}");
        *last = Some(std::time::Instant::now());
    }
}
#[cfg(target_os = "macos")]
enum Epoch {
    Tiered,
    Retired,
    KeptHot,
    /// A reader held the lifetime lock through `wait` before any work started: who (`compact::readers_held`).
    Held(String),
    Stopped,
}
/// One closed <=60 s epoch: decode, re-encode at 1 fps, verify, store cold, swap references, collect, reclaim.
#[cfg(target_os = "macos")]
fn tier_epoch(root: &Path, db: &rusqlite::Connection, queue: &crate::live::Queue, id: i64, wait: std::time::Duration, more: &mut dyn FnMut() -> bool) -> Result<Epoch> {
    // The swap needs the exclusive lifetime lock; while a reader holds it, decoding would be wasted. Probed before this
    // epoch's own shared lock (flock locks are per open file, so a held shared lock here would refuse the probe).
    if !readers_gone(root, wait, more)? {
        if !more() {
            return Ok(Epoch::Stopped);
        }
        return Ok(Epoch::Held(compact::readers_held()));
    }
    let reader = compact::reader_lock(root)?;
    let Some(seg)=db.query_row("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE id=?1 AND tier='hot' AND closed=1",[id],row).optional()? else {return Ok(Epoch::KeptHot)};
    let old = samples(root, seg.id, u64::MAX >> 1)?;
    if old.is_empty() {
        drop(reader);
        let n = queue.background(move |tx| Ok(tx.execute("UPDATE video_segments SET tier='archive' WHERE id=?1 AND tier='hot' AND closed=1 AND NOT EXISTS(SELECT 1 FROM video_samples WHERE segment=?1)", [id])?))?;
        return Ok(if n == 1 { Epoch::Retired } else { Epoch::KeptHot });
    }
    let hot_bytes: usize = old.iter().map(|s| s.data.len()).sum();
    let mut decoder = native::Decoder::new(&seg.format)?;
    let mut encoder = native::Encoder::new(seg.format.width, seg.format.height, 1, 0.5)?;
    let mut cold = vec![];
    let mut format = None;
    let mut next = seg.start_ns;
    for sample in &old {
        if !more() {
            return Ok(Epoch::Stopped);
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
        if !more() {return Ok(Epoch::Stopped);}
        let rgb = check.decode(s)?;
        ensure!(
            rgb.len() == (format.width as usize * format.height as usize * 3),
            "archival video verification failed"
        );
    }
    if !more() {return Ok(Epoch::Stopped);}
    let pack = store::month(seg.wall_ns);
    let items: Vec<_> = cold
        .iter()
        .map(|s| (key(&hash(&s.data)), s.data.as_slice()))
        .collect();
    store::put(root, &pack, &items)?;
    let (cold_keys, cold_pack): (Vec<String>, String) = (items.iter().map(|(k, _)| k.clone()).collect(), pack.clone());
    drop(reader);
    if !more() {return Ok(Epoch::Stopped);}
    // The reference swap is one writer job. It runs only while no reader holds the archive (a pinned command
    // keeps its frames hot), re-checks the epoch, and queues the hot blobs for collection. A reader met at the swap
    // is waited out (`HELD_WAIT`) and the job sent again, outside the writer, so live writes never wait for a reader.
    let (format, cold) = (serde_json::to_string(&format)?, std::sync::Arc::new(cold));
    let swap_started = std::time::Instant::now();
    let swapped = loop {
        let (r, format, cold, pack) = (root.to_path_buf(), format.clone(), cold.clone(), pack.clone());
        let swapped = queue.background(move |tx| swap(tx, &r, id, &pack, &format, &cold))?;
        if let Some(swapped) = swapped {
            break Some(swapped);
        }
        #[cfg(test)]
        SWAPS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let left = wait.saturating_sub(swap_started.elapsed());
        if left.is_zero() || !readers_gone(root, left, more)? {
            // Readers outlasted the wait: the next epochs would meet them too (review, 13 Nov).
            break None;
        }
    };
    if swapped != Some(true) {
        // The cold blobs just written to the month pack are referenced by nothing (review of the night pass, 13 Nov,
        // non-blocker 6): queue them for collection, which deletes only blobs no row references (`store::referenced`),
        // so a blob another epoch shares stays, and a later pass writes them again.
        if let Err(e) = queue.background(move |tx| {
            for k in &cold_keys {
                store::queue_gc(tx, &cold_pack, k)?;
            }
            Ok(())
        }) {
            eprintln!("Archive video epoch {id} kept hot; its unused cold blobs are swept later: {e:#}");
        }
        if swapped.is_none() && more() {
            return Ok(Epoch::Held(compact::readers_held()));
        }
        return Ok(Epoch::KeptHot);
    }
    // The hot blobs go now and their pages back to the disk: before 13 Nov only the audio path reclaimed the hot pack
    // (8192 pages per audio group), so freed video pages were only reused, never returned.
    // The epoch is tiered whatever happens here; a failure only delays the space (the next pass collects again).
    let reclaimed = compact::collect_all(queue, root, more)
        .and_then(|_| store::reclaim_pack(root, store::HOT, ((hot_bytes / 4096) as u32).saturating_add(1024).max(8192)));
    if let Err(e) = reclaimed {
        eprintln!("Archive video epoch {id} tiered; its hot space is reclaimed later: {e:#}");
    }
    Ok(Epoch::Tiered)
}

/// One epoch's reference swap inside a writer job: `None` while a reader holds the archive, `Some(false)` when the
/// epoch changed meanwhile, `Some(true)` once its samples point at the cold blobs.
#[cfg(target_os = "macos")]
fn swap(tx: &rusqlite::Connection, root: &Path, id: i64, pack: &str, format: &str, cold: &[Sample]) -> Result<Option<bool>> {
    let Some(_lock) = compact::exclusive_lock(root)? else { return Ok(None) };
    let unchanged: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM video_segments WHERE id=?1 AND tier='hot' AND closed=1)", [id], |r| r.get(0))?;
    if !unchanged {
        return Ok(Some(false));
    }
    let refs = tx
        .prepare("SELECT pack,sha256 FROM video_samples WHERE segment=?1")?
        .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (p, sha) in refs {
        store::queue_gc(tx, &p, &key(&sha))?;
    }
    tx.execute("DELETE FROM video_samples WHERE segment=?1", [id])?;
    for s in cold {
        let sha = hash(&s.data);
        ensure!(store::has(root, pack, &key(&sha))?, "archival video blob missing");
        tx.execute("INSERT INTO video_samples VALUES(?1,?2,?3,?4,?5,?6)", params![id, s.host_ns as i64, s.end_ns as i64, s.key, sha, pack])?;
    }
    tx.execute("UPDATE video_segments SET tier='archive',fps=1,format=?2 WHERE id=?1", params![id, format])?;
    Ok(Some(true))
}

/// Swaps refused because a reader held the archive (tests).
#[cfg(test)]
static SWAPS_REFUSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
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
        // A helper the crash test runs as a subprocess; on its own (`--run-ignored all`) it has nothing to do.
        let Some(root) = std::env::var_os("HEIMDALL_VIDEO_CRASH_ROOT") else { return };
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
            tier_while(root.path(), DAY_NS + 5_000_000_000, PASS_EPOCHS, &mut || false).unwrap().tiered,
            0
        );
        assert_eq!(
            tier_while(root.path(), DAY_NS + 5_000_000_000, PASS_EPOCHS, &mut || true).unwrap().tiered,
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
    /// One closed epoch of `frames` noise frames at 5 fps starting at `host_ns` (wall time 1 s), on display 1.
    #[cfg(target_os = "macos")]
    fn noise_epoch(writer: &Writer, host_ns: u64, frames: u64, seed: u32) -> i64 {
        let (w, h) = (256u32, 128u32);
        let mut encoder = native::Encoder::new(w, h, 5, 0.8).unwrap();
        let mut x = seed.wrapping_mul(2654435761).max(1);
        let mut id = None;
        for i in 0..frames {
            let rgb: Vec<u8> = (0..w * h * 3).map(|_| { x ^= x << 13; x ^= x >> 17; x ^= x << 5; x as u8 }).collect();
            let (format, sample) = encoder.encode(&rgb, host_ns + i * 200_000_000, host_ns + (i + 1) * 200_000_000).unwrap();
            let id = *id.get_or_insert_with(|| writer.begin(&Geometry { display: 1, x: 0., y: 0., width: w, height: h }, &format, sample.host_ns, 1, 5).unwrap());
            writer.append(id, &sample).unwrap();
        }
        let id = id.unwrap();
        writer.close(id).unwrap();
        id
    }
    fn tier_of(root: &Path, id: i64) -> String {
        store::open(root).unwrap().query_row("SELECT tier FROM video_segments WHERE id=?1", [id], |r| r.get(0)).unwrap()
    }
    /// Disk audit 12 Nov: from 2 Nov 15:51 four closed, empty epochs (an epoch whose first row met a busy archive) were
    /// the four oldest hot rows; tiering took the oldest four, skipped each without retiring it, and tiered nothing
    /// for ten days while hot.sqlite grew to 90 GB. Five empty epochs ahead of one real one: the real one is tiered,
    /// the empty ones retired, and the hot pack gives its pages back.
    #[cfg(target_os = "macos")]
    #[test]
    fn empty_epochs_never_block_tiering_and_the_hot_pack_shrinks() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-empty")).unwrap();
        let geometry = Geometry { display: 1, x: 0., y: 0., width: 256, height: 128 };
        let format = Format { width: 256, height: 128, hvcc: vec![], parameters: vec![] };
        let empty: Vec<i64> = (0..5)
            .map(|i| {
                let id = writer.begin(&geometry, &format, 1_000_000_000 + i * 10_000_000, 0, 5).unwrap();
                writer.close(id).unwrap();
                id
            })
            .collect();
        let real = noise_epoch(&writer, 2_000_000_000, 15, 7);
        let hot_pages = || {
            let p = rusqlite::Connection::open(store::pack_path(root.path(), store::HOT).unwrap()).unwrap();
            let pages: i64 = p.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
            let free: i64 = p.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap();
            (pages, free)
        };
        let (before, _) = hot_pages();
        // The old selection, unchanged: the oldest four hot epochs are all empty.
        let oldest: Vec<i64> = store::open(root.path()).unwrap()
            .prepare("SELECT id FROM video_segments WHERE tier='hot' AND closed=1 ORDER BY wall_ns LIMIT 4").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert!(oldest.iter().all(|id| empty.contains(id)), "fixture: empty epochs come first");
        let t = tier_while(root.path(), DAY_NS + 20_000_000_000, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!((t.tiered, t.retired, t.errors.len()), (1, 5, 0), "{t:?}");
        assert_eq!(tier_of(root.path(), real), "archive");
        for id in &empty {
            assert_eq!(tier_of(root.path(), *id), "archive");
        }
        let (after, free) = hot_pages();
        assert_eq!(free, 0, "the hot pack's freed pages go back to the disk");
        assert!(after < before, "hot pack {before} -> {after} pages");
        let cold = at(&root.path().join("session-empty"), 1, 2_100_000_000).unwrap().unwrap();
        assert_eq!((cold.0.tier.as_str(), cold.0.fps), ("archive", 1));
        // Nothing left: a second call does nothing and retires nothing.
        let again = tier_while(root.path(), DAY_NS + 20_000_000_000, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!((again.tiered, again.retired), (0, 0));
    }
    /// Review, 13 Nov: while a reader held the archive, every epoch of the backlog was decoded, re-encoded and written
    /// cold only to be refused at its swap. A reader held through the whole wait stops the pass before any work; it
    /// writes nothing and tiers after the reader.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_held_archive_stops_the_pass_before_any_work() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-held")).unwrap();
        let ids: Vec<i64> = (0..3).map(|i| noise_epoch(&writer, 1_000_000_000 + i * 2_000_000_000, 2, i as u32 + 1)).collect();
        let now = DAY_NS + 100_000_000_000;
        let reader = compact::reader_lock(root.path()).unwrap();
        let t = tier_waiting(root.path(), now, PASS_EPOCHS, std::time::Duration::from_millis(200), &mut || true).unwrap();
        assert_eq!((t.tiered, t.errors.len()), (0, 0), "{t:?}");
        let month = store::month(1);
        assert!(!store::pack_path(root.path(), &month).unwrap().exists(), "nothing was encoded into the month pack");
        drop(reader);
        let t = tier_while(root.path(), now, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!(t.tiered, ids.len(), "{t:?}");
    }
    /// 13 Nov: the search indexer held the archive's reader lock almost without a break (it let go for under a third
    /// of a second between groups), and a pass gave up at its first refused probe, so from 10:22 to 20:03 no epoch
    /// was tiered while the backlog grew to 229. A reader met at an epoch's start and again at its swap now only
    /// delays the pass.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_reader_that_comes_and_goes_delays_tiering_but_never_stops_it() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-busy")).unwrap();
        let ids: Vec<i64> = (0..3).map(|i| noise_epoch(&writer, 1_000_000_000 + i * 2_000_000_000, 2, i as u32 + 1)).collect();
        let hold = |lock: compact::ReaderLock| {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(lock);
            })
        };
        // Held when the pass starts.
        let first = hold(compact::reader_lock(root.path()).unwrap());
        // Taken again between the cold write and the swap.
        let (r, month) = (root.path().to_path_buf(), store::month(1));
        let mut second = None;
        let mut more = || {
            if second.is_none() && store::pack_path(&r, &month).unwrap().exists() {
                second = Some(hold(compact::reader_lock(&r).unwrap()));
            }
            true
        };
        let refused = SWAPS_REFUSED.load(std::sync::atomic::Ordering::Relaxed);
        let t = tier_while(root.path(), DAY_NS + 100_000_000_000, PASS_EPOCHS, &mut more).unwrap();
        assert_eq!((t.tiered, t.errors.len()), (ids.len(), 0), "{t:?}");
        assert!(second.is_some(), "a reader came back before the swap");
        assert!(SWAPS_REFUSED.load(std::sync::atomic::Ordering::Relaxed) > refused, "the swap met the reader and was sent again");
        first.join().unwrap();
        second.unwrap().join().unwrap();
        for id in ids {
            assert_eq!(tier_of(root.path(), id), "archive");
        }
    }
    /// Review of the night pass (13 Nov), non-blocker 5: an epoch that keeps failing is parked after `PARK_AFTER`
    /// failures, reported once with its reason, and never decoded again; its hot footage stays.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_epoch_that_keeps_failing_is_parked_and_reported_once() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-park")).unwrap();
        let bad = noise_epoch(&writer, 1_000_000_000, 2, 1);
        {
            let db = store::open(root.path()).unwrap();
            let p = rusqlite::Connection::open(store::pack_path(root.path(), store::HOT).unwrap()).unwrap();
            let sha: String = db.query_row("SELECT sha256 FROM video_samples WHERE segment=?1 ORDER BY host_ns LIMIT 1", [bad], |r| r.get(0)).unwrap();
            p.execute("DELETE FROM blobs WHERE key=?1", [key(&sha)]).unwrap();
        }
        let now = DAY_NS + 200_000_000_000;
        let key_root = root.path().canonicalize().unwrap();
        let mut lines = vec![];
        for _ in 0..PARK_AFTER + 2 {
            // `FAILED_RETRY` has passed.
            failed().lock().unwrap().retain(|(r, _), _| *r != key_root);
            lines.extend(tier_while(root.path(), now, PASS_EPOCHS, &mut || true).unwrap().errors);
        }
        assert_eq!(lines.len(), PARK_AFTER as usize, "{lines:?}");
        assert!(lines.last().unwrap().contains("parked") && lines.last().unwrap().contains("missing"), "{lines:?}");
        assert!(!lines[0].contains("parked"));
        assert_eq!(tier_of(root.path(), bad), "hot", "a parked epoch keeps its hot footage");
        let db = store::open(root.path()).unwrap();
        let (n, reason): (i64, String) = db.query_row("SELECT failures,reason FROM video_tier_failures WHERE segment=?1", [bad], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(n, PARK_AFTER);
        assert!(reason.contains("missing"), "{reason}");
    }
    /// Review of the night pass (13 Nov), non-blocker 6: a swap refused after the cold blobs were written queues them
    /// for collection, and collection removes only those nothing references.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_swap_queues_its_cold_blobs_for_collection() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-refused")).unwrap();
        let id = noise_epoch(&writer, 1_000_000_000, 2, 1);
        let queue = crate::live::Queue::shared(root.path()).unwrap();
        let db = store::open(root.path()).unwrap();
        // A reader takes the archive between the cold write and the swap: the swap's exclusive lock is refused.
        let mut reader = None;
        let mut asked = 0;
        let r = root.path().to_path_buf();
        let month = store::month(1);
        let mut more = || {
            asked += 1;
            if reader.is_none() && store::pack_path(&r, &month).unwrap().exists() {
                reader = Some(compact::reader_lock(&r).unwrap());
            }
            true
        };
        assert!(matches!(tier_epoch(root.path(), &db, &queue, id, std::time::Duration::from_millis(200), &mut more).unwrap(), Epoch::Held(_)));
        assert!(asked > 0);
        drop(reader);
        assert_eq!(tier_of(root.path(), id), "hot");
        let queued: i64 = db.query_row("SELECT count(*) FROM gc_blobs WHERE pack=?1", [&month], |r| r.get(0)).unwrap();
        assert!(queued > 0, "the unreferenced cold blobs are queued");
        compact::collect_all(&queue, root.path(), &mut || true).unwrap();
        let left: i64 = rusqlite::Connection::open(store::pack_path(root.path(), &month).unwrap()).unwrap()
            .query_row("SELECT count(*) FROM blobs", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 0, "collected");
        // The epoch still tiers on the next pass.
        let t = tier_while(root.path(), DAY_NS + 100_000_000_000, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!(t.tiered, 1, "{t:?}");
    }
    /// An epoch that cannot be tiered (here a hot blob gone) is reported and passed over; later epochs still tier,
    /// and it is not tried again within `FAILED_RETRY`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_failing_epoch_is_reported_and_passed_over() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-bad")).unwrap();
        let bad: Vec<i64> = (0..PASS_EPOCHS as u64 + 1).map(|i| noise_epoch(&writer, 1_000_000_000 + i * 2_000_000_000, 2, i as u32 + 1)).collect();
        let good = noise_epoch(&writer, 100_000_000_000, 3, 99);
        {
            let db = store::open(root.path()).unwrap();
            let p = rusqlite::Connection::open(store::pack_path(root.path(), store::HOT).unwrap()).unwrap();
            for id in &bad {
                let sha: String = db.query_row("SELECT sha256 FROM video_samples WHERE segment=?1 ORDER BY host_ns LIMIT 1", [id], |r| r.get(0)).unwrap();
                p.execute("DELETE FROM blobs WHERE key=?1", [key(&sha)]).unwrap();
            }
        }
        let now = DAY_NS + 200_000_000_000;
        let t = tier_while(root.path(), now, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!(t.tiered, 1, "{t:?}");
        assert_eq!(t.errors.len(), bad.len(), "{:?}", t.errors);
        assert!(t.errors[0].contains("missing"), "{}", t.errors[0]);
        assert_eq!(tier_of(root.path(), good), "archive");
        assert!(bad.iter().all(|id| tier_of(root.path(), *id) == "hot"), "a failed epoch keeps its hot rows");
        let again = tier_while(root.path(), now, PASS_EPOCHS, &mut || true).unwrap();
        assert_eq!((again.tiered, again.errors.len()), (0, 0), "failed epochs wait for their retry");
    }

    /// Representative frames asked in time order decode their epoch forward once, with the pixels of a fresh decode
    /// per frame; a frame earlier than the decoder, another epoch, or a re-encoded epoch starts a new decoder.
    #[cfg(target_os = "macos")]
    #[test]
    fn frames_decode_an_epoch_forward_once_with_fresh_decode_pixels() {
        let root = tempfile::tempdir().unwrap();
        let writer = Writer::open(&root.path().join("session-video")).unwrap();
        let geometry = Geometry { display: 1, x: 0., y: 0., width: 64, height: 32 };
        const S: u64 = 200_000_000;
        let mut epochs = vec![];
        for epoch in 0..2u64 {
            let mut encoder = native::Encoder::new(64, 32, 5, 0.8).unwrap();
            let start = 1_000_000_000 + epoch * 100 * S;
            let mut id = 0;
            for i in 0..12u64 {
                let rgb: Vec<u8> = (0..64 * 32u64).flat_map(|p| [(i * 20 + epoch * 7) as u8, (p % 64 * 4) as u8, (p / 64 * 8) as u8]).collect();
                let (format, sample) = encoder.encode(&rgb, start + i * S, start + (i + 1) * S).unwrap();
                if i == 0 { id = writer.begin(&geometry, &format, start, 1, 5).unwrap(); }
                writer.append(id, &sample).unwrap();
            }
            epochs.push((id, start));
        }
        let session = segments(root.path(), 1).map(|s| s[0].session).unwrap_or(1);
        let out = tempfile::tempdir().unwrap();
        let fresh = |id: i64, host: u64| {
            let path = out.path().join(format!("fresh-{id}-{host}.png"));
            representative_png(root.path(), session, id, host, &path, &mut || true).unwrap();
            image::open(path).unwrap().to_rgb8()
        };
        let mut frames = Frames::default();
        let got = |frames: &mut Frames, id: i64, host: u64| {
            let path = out.path().join(format!("shared-{id}-{host}.png"));
            frames.png(root.path(), session, id, host, &path, &mut || true).unwrap();
            image::open(path).unwrap().to_rgb8()
        };
        let (a, start) = epochs[0];
        // Frames 3, 3 again, 7 and 11 of one epoch: 12 samples decoded, not 4 + 4 + 8 + 12.
        for (n, decoded) in [(3, 4), (3, 4), (7, 8), (11, 12)] {
            let host = start + n * S + S / 2;
            assert_eq!(got(&mut frames, a, host), fresh(a, host), "frame {n}");
            assert_eq!(frames.decoded, decoded, "frame {n}");
        }
        // Behind the decoder: a new decoder from the epoch's start.
        assert_eq!(got(&mut frames, a, start + 2 * S), fresh(a, start + 2 * S));
        assert_eq!(frames.decoded, 15);
        // Another epoch: its own decoder.
        let (b, start_b) = epochs[1];
        assert_eq!(got(&mut frames, b, start_b + 5 * S), fresh(b, start_b + 5 * S));
        assert_eq!(frames.decoded, 21);
        // A re-encoded epoch holds other samples: the decoder is not resumed over them.
        let db = store::open(root.path()).unwrap();
        db.execute("UPDATE video_samples SET sha256='0'||substr(sha256,2) WHERE segment=?1 AND host_ns=?2", params![b, (start_b + 2 * S) as i64]).unwrap();
        let resumed = frames.png(root.path(), session, b, start_b + 8 * S, &out.path().join("changed.png"), &mut || true);
        assert!(format!("{:#}", resumed.unwrap_err()).contains("missing"), "the changed sample is read again, not skipped");
    }
}
