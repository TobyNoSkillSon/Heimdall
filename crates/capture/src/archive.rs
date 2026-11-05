use crate::{NativeFormat, SourceId, SourceInfo};
use anyhow::{Context, Result};
use heimdall_archive::{audio, store};
use rusqlite::{Connection, params};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
#[derive(Debug)]
pub struct ArchivedBlock {
    pub source_id: SourceId,
    pub host_ns: u64,
    pub format: NativeFormat,
    pub samples: Vec<f32>,
    pub discontinuity: bool,
}
/// One capture timeline (segment) of a session in the archive store. Float32 + zstd level 1:
/// bit-exact, no VAD gate, no quantization of quiet speech. Exactly +0.0 runs have only a frame
/// count; -0 and every nonzero bit remain in compressed PCM. Callback rows are buffered and
/// committed about once a second: hot blobs first, then one short index transaction by the store's writer
/// (`heimdall_archive::live`), which the committer waits for.
pub struct Archive {
    db: Connection,
    queue: heimdall_archive::live::Queue,
    root: PathBuf,
    session: PathBuf,
    segment: i64,
    pending: usize,
    last_commit: std::time::Instant,
    ops: Vec<Op>,
    silence: HashMap<SourceId, Silence>,
    buffered: HashMap<SourceId, PendingPcm>,
    /// Sealed one-second PCM batches awaiting their object commit at flush.
    sealed: Vec<(String, NativeFormat, Vec<f32>)>,
    read_only: bool,
}
#[derive(Clone)]
enum Op {
    Source { id: String, json: String, ns: u64 },
    Remove { id: String, ns: u64 },
    Gap { id: Option<String>, ns: u64, detail: String },
    Block(Row),
    ExtendSilence { row: i64, frames: u64 },
}
#[derive(Clone)]
struct Row {
    source: String,
    host_ns: u64,
    format: NativeFormat,
    frames: u64,
    zero: bool,
    discontinuity: bool,
    object: Option<String>,
    offset: usize,
    wall_ns: u64,
}
/// A contiguous exact-zero run: its row (buffered op index, or committed row ID), start and length.
#[derive(Clone, Copy)]
struct Silence {
    at: SilenceRow,
    start: u64,
    frames: u64,
    format: NativeFormat,
}
#[derive(Clone, Copy)]
enum SilenceRow {
    Buffered(usize),
    Committed { row: i64, dirty: bool },
}
struct PendingPcm {
    format: NativeFormat,
    samples: Vec<f32>,
    rows: Vec<(usize, usize)>,
}
impl Archive {
    /// A new capture segment of `session` (`<archive>/<session-name>`).
    pub fn create(session: &Path) -> Result<Self> {
        let (root, name) = store::locate(session)?;
        let queue = heimdall_archive::live::Queue::shared(&root)?;
        let db = store::open(&root)?;
        let session_id = store::ensure_session(&db, &name)?;
        // Wall and host clocks read together, after the open: the open may wait for the archive lock, and the
        // segment's wall anchor is its host origin's wall time.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
        let segment = store::create_segment(
            &db,
            session_id,
            &format!("capture-{stamp}-{}", std::process::id()),
            stamp,
            crate::host_now_ns(),
            "f32-planar-zstd-1 / exact +0 runs",
        )?;
        Ok(Self {
            db,
            queue,
            root,
            session: session.into(),
            segment,
            pending: 0,
            last_commit: std::time::Instant::now(),
            ops: vec![],
            silence: HashMap::new(),
            buffered: HashMap::new(),
            sealed: vec![],
            read_only: false,
        })
    }
    /// A session's capture timelines for reconstruction, without starting a new one.
    pub fn open_read_only(session: &Path) -> Result<Self> {
        let (root, name) = store::locate(session)?;
        let db = store::open(&root)?;
        store::find_session(&db, &name)?.with_context(|| format!("archive has no session {name}"))?;
        Ok(Self {
            db,
            queue: heimdall_archive::live::Queue::shared(&root)?,
            root,
            session: session.into(),
            segment: 0,
            pending: 0,
            last_commit: std::time::Instant::now(),
            ops: vec![],
            silence: HashMap::new(),
            buffered: HashMap::new(),
            sealed: vec![],
            read_only: true,
        })
    }
    /// The session this capture belongs to.
    pub fn path(&self) -> &Path {
        &self.session
    }
    /// This capture timeline's segment ID in the store.
    pub fn segment(&self) -> i64 {
        self.segment
    }
    pub fn add_source(&mut self, info: &SourceInfo, host_ns: u64) -> Result<()> {
        self.ops.push(Op::Source { id: info.id.0.clone(), json: serde_json::to_string(info)?, ns: host_ns });
        self.tick()
    }
    pub fn remove_source(&mut self, id: &SourceId, host_ns: u64) -> Result<()> {
        self.seal_source(id)?;
        self.end_silence(id);
        self.ops.push(Op::Remove { id: id.0.clone(), ns: host_ns });
        self.tick()
    }
    pub fn gap(&mut self, id: Option<&SourceId>, host_ns: u64, detail: &str) -> Result<()> {
        if let Some(id) = id {
            self.end_silence(id);
        }
        self.ops.push(Op::Gap { id: id.map(|i| i.0.clone()), ns: host_ns, detail: detail.into() });
        self.tick()
    }
    pub fn append(
        &mut self,
        id: &SourceId,
        host_ns: u64,
        format: NativeFormat,
        samples: &[f32],
        discontinuity: bool,
    ) -> Result<()> {
        format.validate()?;
        anyhow::ensure!(samples.len().is_multiple_of(format.channels as usize), "partial native frame");
        let frames = (samples.len() / format.channels as usize) as u64;
        if frames == 0 {
            return Ok(());
        }
        let zero = samples.iter().all(|s| s.to_bits() == 0);
        if zero && !discontinuity {
            if let Some(run) = self.silence.get_mut(id) {
                let end = run.start + run.frames * 1_000_000_000 / format.sample_rate as u64;
                // Merge only genuinely contiguous digital silence; any native time
                // hole keeps its own block. This never substitutes VAD silence.
                if run.frames + frames <= format.sample_rate as u64 * 3600
                    && host_ns.abs_diff(end) <= 1_000_000_000 / format.sample_rate as u64
                    && run.format == format
                {
                    run.frames += frames;
                    match &mut run.at {
                        SilenceRow::Buffered(i) => {
                            if let Op::Block(row) = &mut self.ops[*i] {
                                row.frames = run.frames;
                            }
                        }
                        SilenceRow::Committed { dirty, .. } => *dirty = true,
                    }
                    return self.tick();
                }
            }
        }
        if self.buffered.get(id).is_some_and(|p| {
            p.format != format
                || p.samples.len() + samples.len() > format.sample_rate as usize * format.channels as usize
        }) {
            self.seal_source(id)?;
        }
        self.end_silence(id);
        let wall_ns = heimdall_archive::wall_ns().saturating_sub(crate::host_now_ns().saturating_sub(host_ns));
        let index = self.ops.len();
        self.ops.push(Op::Block(Row {
            source: id.0.clone(),
            host_ns,
            format,
            frames,
            zero,
            discontinuity,
            object: None,
            offset: 0,
            wall_ns,
        }));
        if !zero {
            let pending = self.buffered.entry(id.clone()).or_insert_with(|| PendingPcm { format, samples: vec![], rows: vec![] });
            pending.rows.push((index, pending.samples.len() / format.channels as usize));
            pending.samples.extend_from_slice(samples);
        }
        if zero && !discontinuity {
            self.silence.insert(id.clone(), Silence { at: SilenceRow::Buffered(index), start: host_ns, frames, format });
        }
        self.tick()
    }
    /// Ending a committed zero run must retain its partial-second extension until
    /// flush. Buffered runs already have their full length in their block op.
    fn end_silence(&mut self, id: &SourceId) {
        if let Some(Silence { at: SilenceRow::Committed { row, dirty: true }, frames, .. }) = self.silence.remove(id) {
            self.ops.push(Op::ExtendSilence { row, frames });
        }
    }
    fn tick(&mut self) -> Result<()> {
        self.pending += 1;
        if self.last_commit.elapsed() >= std::time::Duration::from_secs(1) {
            if let Err(e) = self.flush() {
                // Behind a held lock the next attempt is a second away, not on every callback.
                self.last_commit = std::time::Instant::now();
                return Err(e);
            }
        }
        Ok(())
    }
    pub(crate) fn commit_wait(&self) -> Option<std::time::Duration> {
        (self.pending > 0).then(|| std::time::Duration::from_secs(1).saturating_sub(self.last_commit.elapsed()))
    }
    /// Turns a source's buffered PCM into one object reference for its rows. The object itself is
    /// encoded and committed at flush.
    fn seal_source(&mut self, id: &SourceId) -> Result<()> {
        let Some(p) = self.buffered.remove(id) else { return Ok(()) };
        let object = audio::native_id(p.format.sample_rate, p.format.channels, &p.samples);
        for (index, offset) in &p.rows {
            if let Op::Block(row) = &mut self.ops[*index] {
                row.object = Some(object.clone());
                row.offset = *offset;
            }
        }
        self.sealed.push((object, p.format, p.samples));
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        anyhow::ensure!(!self.read_only, "archive session was opened read-only");
        let sources: Vec<_> = self.buffered.keys().cloned().collect();
        for id in sources {
            self.seal_source(&id)?;
        }
        let dirty = self.silence.values().any(|s| matches!(s.at, SilenceRow::Committed { dirty: true, .. }));
        if self.ops.is_empty() && !dirty {
            self.pending = 0;
            self.last_commit = std::time::Instant::now();
            return Ok(());
        }
        // Hot blobs first (one durable pack transaction), then the index rows that reference them, by the store's
        // writer. Any error, a full disk included, keeps the sealed audio for the next flush: the rows in `ops` already
        // name its objects (31 Oct 22:22:31: a 60 s wait for the lock ended here). A held lock is waited out by the
        // writer, never returned.
        let mut objects: Vec<audio::Encoded> = vec![];
        let mut collections = 0;
        let sealed = std::mem::take(&mut self.sealed);
        let ready = (|| -> Result<()> {
            // Read before the blobs are written: unchanged under the writer's lock, no collection removed them since,
            // and the writer catalogues them without reading the hot pack (`audio::publish_put`).
            collections = store::collections(&self.db)?;
            for (id, format, samples) in &sealed {
                if objects.iter().any(|o| o.id == *id) || audio::catalogued(&self.db, id)? {
                    continue;
                }
                objects.push(audio::encode_lossless(format.sample_rate, format.channels, samples)?);
            }
            let items: Vec<(String, &[u8])> = objects.iter().map(|o| (store::audio_key(&o.id), o.bytes.as_slice())).collect();
            store::put(&self.root, store::HOT, &items)
        })();
        if let Err(e) = ready {
            self.sealed = sealed;
            return Err(e);
        }
        let dirty: Vec<(i64, u64)> = self
            .silence
            .values()
            .filter_map(|s| match s.at {
                SilenceRow::Committed { row, dirty: true } => Some((row, s.frames)),
                _ => None,
            })
            .collect();
        let job = Commit { segment: self.segment, root: self.root.clone(), ops: self.ops.clone(), objects, collections, sealed: std::sync::Arc::new(sealed), dirty };
        let sealed = job.sealed.clone();
        let zero_rows = match self.queue.call(move |db| job.apply(db)) {
            Ok(rows) => rows,
            Err(e) => {
                self.sealed = std::sync::Arc::try_unwrap(sealed).unwrap_or_else(|s| (*s).clone());
                return Err(e);
            }
        };
        let zero_rows: Vec<(SourceId, i64)> = zero_rows
            .into_iter()
            .filter_map(|(index, row)| {
                self.silence.iter().find(|(_, s)| matches!(s.at, SilenceRow::Buffered(i) if i == index)).map(|(id, _)| (id.clone(), row))
            })
            .collect();
        for s in self.silence.values_mut() {
            if let SilenceRow::Committed { dirty, .. } = &mut s.at {
                *dirty = false;
            }
        }
        for (id, row) in zero_rows {
            if let Some(s) = self.silence.get_mut(&id) {
                s.at = SilenceRow::Committed { row, dirty: false };
            }
        }
        self.ops.clear();
        self.pending = 0;
        self.last_commit = std::time::Instant::now();
        Ok(())
    }
    /// Committed blocks of one source across the session's capture timelines, live and packed.
    pub fn read_blocks(&self, id: &SourceId) -> Result<Vec<ArchivedBlock>> {
        let view = heimdall_archive::View::session(&self.session)?;
        view.capture()?;
        let rows: Vec<(i64, i64, bool)> = view
            .db
            .prepare("SELECT id,host_ns,discontinuity FROM temp.blocks WHERE source_id=?1 ORDER BY id")?
            .query_map([&id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut cache = audio::ObjectCache::default();
        let mut blocks = Vec::with_capacity(rows.len());
        for (row, ns, discontinuity) in rows {
            let pcm = audio::read_block_cached(&view.db, row, &mut cache)?;
            blocks.push(ArchivedBlock {
                source_id: id.clone(),
                host_ns: ns as u64,
                format: NativeFormat { sample_rate: pcm.rate, channels: pcm.channels },
                samples: pcm.samples,
                discontinuity,
            });
        }
        Ok(blocks)
    }
}
/// One flush's index rows, applied by the store's writer (and again, unchanged, if a held lock or a full disk rolled
/// its transaction back). Returns the committed row of each buffered exact-zero block, by op index.
struct Commit {
    segment: i64,
    root: PathBuf,
    ops: Vec<Op>,
    /// Blobs this flush committed to the hot pack, and `store::collections` read before it did.
    objects: Vec<audio::Encoded>,
    collections: i64,
    sealed: std::sync::Arc<Vec<(String, NativeFormat, Vec<f32>)>>,
    dirty: Vec<(i64, u64)>,
}
impl Commit {
    fn apply(&self, db: &Connection) -> Result<Vec<(usize, i64)>> {
        audio::publish_put(db, &self.root, store::HOT, &self.objects, self.collections)?;
        // An object seen as catalogued before may have been collected since: its blob is written here.
        let mut late = vec![];
        for (id, format, samples) in self.sealed.iter() {
            if !self.objects.iter().any(|o| o.id == *id) && !late.iter().any(|o: &audio::Encoded| o.id == *id) && !audio::catalogued(db, id)? {
                late.push(audio::encode_lossless(format.sample_rate, format.channels, samples)?);
            }
        }
        audio::publish(db, &self.root, store::HOT, &late)?;
        let mut zero_rows = vec![];
        for (index, op) in self.ops.iter().enumerate() {
            match op {
                Op::Source { id, json, ns } => {
                    db.prepare_cached("INSERT OR REPLACE INTO sources VALUES(?1,?2,?3,?4,NULL)")?.execute(params![self.segment, id, json, *ns as i64])?;
                }
                Op::Remove { id, ns } => {
                    db.prepare_cached("UPDATE sources SET removed_ns=?3 WHERE segment=?1 AND id=?2")?.execute(params![self.segment, id, *ns as i64])?;
                }
                Op::Gap { id, ns, detail } => {
                    db.prepare_cached("INSERT INTO gaps(segment,source_id,host_ns,detail) VALUES(?1,?2,?3,?4)")?.execute(params![self.segment, id, *ns as i64, detail])?;
                }
                Op::ExtendSilence { row, frames } => {
                    db.prepare_cached("UPDATE blocks SET frames=?2 WHERE id=?1")?.execute(params![row, *frames as i64])?;
                }
                Op::Block(r) => {
                    db.prepare_cached("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?
                        .execute(params![
                            self.segment,
                            r.source,
                            r.host_ns as i64,
                            r.format.sample_rate,
                            r.format.channels,
                            r.frames as i64,
                            if r.zero { "zero" } else { "f32-zstd" },
                            r.discontinuity,
                            r.object,
                            r.offset as i64,
                            r.wall_ns as i64
                        ])?;
                    if r.zero {
                        zero_rows.push((index, db.last_insert_rowid()));
                    }
                }
            }
        }
        // Project one source span per flush, never one semantic chunk per callback.
        let session: i64 = db.query_row("SELECT session FROM segments WHERE id=?1", [self.segment], |r| r.get(0))?;
        let mut spans: HashMap<&str, (u64, u64, u64)> = HashMap::new();
        for op in &self.ops {
            if let Op::Block(r) = op {
                if !r.zero {
                    let end = r.host_ns + r.frames * 1_000_000_000 / r.format.sample_rate as u64;
                    spans.entry(&r.source).and_modify(|s| { s.0 = s.0.min(r.host_ns); s.1 = s.1.max(end); s.2 = s.2.min(r.wall_ns); }).or_insert((r.host_ns, end, r.wall_ns));
                }
            }
        }
        for (source, (start, end, wall)) in spans {
            heimdall_archive::timeline::audio_span(db, session, source, start, end, wall)?;
        }
        for (row, frames) in &self.dirty {
            db.prepare_cached("UPDATE blocks SET frames=?2 WHERE id=?1")?.execute(params![row, *frames as i64])?;
        }
        Ok(zero_rows)
    }
}
impl Drop for Archive {
    fn drop(&mut self) {
        if self.read_only {
            return;
        }
        let _ = self.flush();
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;
    fn session(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("session-capture-test")
    }
    #[test]
    fn committed_silence_keeps_its_unflushed_tail_at_every_boundary() {
        for boundary in ["speech", "negative-zero", "time-hole", "format", "discontinuity", "gap", "remove"] {
            let dir = tempfile::tempdir().unwrap();
            let path = session(&dir);
            let mut a = Archive::create(&path).unwrap();
            let f = NativeFormat { sample_rate: 16000, channels: 1 };
            let source = SourceInfo::new(SourceKind::InputDevice { name: boundary.into(), uid: boundary.into() }, f, 1);
            a.add_source(&source, 0).unwrap();
            a.append(&source.id, 0, f, &[0.; 320], false).unwrap();
            a.flush().unwrap();
            // Explicit commits only: reproduce an unfinished partial-second update
            // without relying on scheduling, sleeps or the one-second timer.
            a.last_commit = std::time::Instant::now() + std::time::Duration::from_secs(3600);
            for tick in 1..15 {
                a.append(&source.id, tick * 20_000_000, f, &[0.; 320], false).unwrap();
            }
            let mut expected = vec![0.; 4800];
            let mut next_ns = 300_000_000;
            let mut next_format = f;
            let mut discontinuity = false;
            let pcm = match boundary {
                "speech" => vec![0.125; 320],
                "negative-zero" => vec![-0.; 320],
                "time-hole" => { next_ns += 20_000_000; vec![0.; 320] }
                "format" => { next_format.channels = 2; vec![0.; 640] }
                "discontinuity" => { discontinuity = true; vec![0.; 320] }
                "gap" => {
                    a.gap(Some(&source.id), next_ns, "boundary").unwrap();
                    vec![0.; 320]
                }
                "remove" => {
                    a.remove_source(&source.id, next_ns).unwrap();
                    vec![]
                }
                _ => unreachable!(),
            };
            if !pcm.is_empty() {
                a.append(&source.id, next_ns, next_format, &pcm, discontinuity).unwrap();
                expected.extend_from_slice(&pcm);
            }
            // Clean finish must also persist the final partial PCM batch/zero run.
            drop(a);
            let reopened = Archive::open_read_only(&path).unwrap();
            let rows = reopened.read_blocks(&source.id).unwrap();
            let actual: Vec<_> = rows.iter().flat_map(|b| b.samples.iter().copied()).collect();
            assert_eq!(actual.len(), expected.len(), "{boundary}");
            assert_eq!(audio::pcm_bytes(&actual), audio::pcm_bytes(&expected), "{boundary}");
            assert_eq!(rows[0].samples.len(), 4800, "{boundary}");
            if !pcm.is_empty() {
                assert_eq!(rows[1].host_ns, next_ns, "{boundary}");
                assert_eq!(rows[1].format, next_format, "{boundary}");
                assert_eq!(rows[1].discontinuity, discontinuity, "{boundary}");
            }
        }
    }
    #[test]
    fn callbacks_share_one_hot_object_without_changing_native_bits() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Archive::create(&session(&dir)).unwrap();
        let format = NativeFormat { sample_rate: 48000, channels: 1 };
        let source = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "packed-native".into() }, format, 1);
        a.add_source(&source, 0).unwrap();
        let mut expected = vec![];
        for i in 0..32u64 {
            let pcm: Vec<f32> = (0..512).map(|n| f32::from_bits(0x3c000001 + n + i as u32 * 512)).collect();
            expected.push(pcm.clone());
            a.append(&source.id, i * 512 * 1_000_000_000 / 48000, format, &pcm, false).unwrap();
        }
        a.flush().unwrap();
        assert_eq!(a.db.query_row("SELECT count(*) FROM audio_objects", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let blocks = a.read_blocks(&source.id).unwrap();
        assert_eq!(blocks.len(), 32);
        for (row, pcm) in blocks.iter().zip(expected) {
            assert_eq!(audio::pcm_bytes(&row.samples), audio::pcm_bytes(&pcm));
        }
    }
    #[test]
    fn flush_holds_the_shared_index_only_briefly_and_rows_wait_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::create(&session(&dir)).unwrap();
        let format = NativeFormat { sample_rate: 16000, channels: 1 };
        let source = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "archive-lock".into() }, format, 1);
        archive.add_source(&source, 0).unwrap();
        archive.append(&source.id, 0, format, &[0.1; 512], false).unwrap();
        // Between flushes capture holds no index transaction: another writer gets in at once.
        let other = Connection::open(dir.path().join(store::INDEX)).unwrap();
        other.execute_batch("BEGIN IMMEDIATE;COMMIT").unwrap();
        archive.flush().unwrap();
        other.execute_batch("BEGIN IMMEDIATE;COMMIT").unwrap();
        drop(other);
        archive.append(&source.id, 32_000_000, format, &[0.2; 512], false).unwrap();
        archive.flush().unwrap();
        assert_eq!(archive.read_blocks(&source.id).unwrap().len(), 2);
    }
    /// 31 Oct 22:22:31: a flush that could not get the lock ended capture ("archive pipeline stopped: database is
    /// locked"), and the sealed audio it had taken for that flush was dropped with the error. Another process holding
    /// the lock now only delays the flush: the writer waits it out, and every callback commits with its audio.
    #[test]
    fn a_flush_behind_a_held_lock_waits_and_keeps_its_audio() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::create(&session(&dir)).unwrap();
        let format = NativeFormat { sample_rate: 16000, channels: 1 };
        let source = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "held-lock".into() }, format, 1);
        archive.add_source(&source, 0).unwrap();
        archive.append(&source.id, 0, format, &[0.25; 512], false).unwrap();
        let path = dir.path().join(store::INDEX);
        let (locked, held) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let holder = Connection::open(path).unwrap();
            holder.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('held','1');").unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(400));
            holder.execute_batch("ROLLBACK").unwrap();
        });
        held.recv().unwrap();
        let _ = archive.append(&source.id, 32_000_000, format, &[0.5; 512], false);
        let started = std::time::Instant::now();
        archive.flush().unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(200), "the flush waited for the lock");
        holder.join().unwrap();
        let blocks = archive.read_blocks(&source.id).unwrap();
        assert_eq!(blocks.len(), 2, "both callbacks committed after the lock freed");
        assert!(blocks.iter().all(|b| b.samples.iter().all(|s| *s != 0.0)), "with their audio");
    }
    #[test]
    fn cold_compaction_runs_while_capture_is_live_and_next_callback_stays_hot() {
        let dir = tempfile::tempdir().unwrap();
        let mut archive = Archive::create(&session(&dir)).unwrap();
        let format = NativeFormat { sample_rate: 16000, channels: 1 };
        let source = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "cold-live".into() }, format, 1);
        archive.add_source(&source, 0).unwrap();
        archive.append(&source.id, 0, format, &[0.25; 16000], false).unwrap();
        archive.flush().unwrap();
        archive.db.execute("UPDATE blocks SET wall_ns=1", []).unwrap();
        let now = heimdall_archive::wall_ns() + heimdall_archive::DAY_NS;
        assert_eq!(heimdall_archive::compact::compact_audio(dir.path(), now, 32000, 1).unwrap().audio_groups, 1);
        heimdall_archive::compact::compact_metadata(dir.path(), now, true).unwrap();
        archive.append(&source.id, 1_000_000_000, format, &[0.5; 512], false).unwrap();
        archive.flush().unwrap();
        let rows = archive.read_blocks(&source.id).unwrap();
        assert_eq!(rows[0].samples, vec![0.25; 16000]);
        assert_eq!(rows[1].samples, vec![0.5; 512]);
        assert_eq!(
            archive.db.query_row("SELECT codec FROM blocks ORDER BY id DESC LIMIT 1", [], |r| r.get::<_, String>(0)).unwrap(),
            "f32-zstd"
        );
    }
    #[test]
    fn silence_batches_sql_but_seals_exact_coverage_at_every_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Archive::create(&session(&dir)).unwrap();
        let f = NativeFormat { sample_rate: 16000, channels: 1 };
        let source = SourceInfo::new(SourceKind::InputDevice { name: "silence".into(), uid: "silence".into() }, f, 1);
        a.add_source(&source, 0).unwrap();
        a.append(&source.id, 0, f, &[0.; 320], false).unwrap();
        let changes = a.db.total_changes();
        for i in 1..20 {
            a.append(&source.id, i * 20_000_000, f, &[0.; 320], false).unwrap();
        }
        assert_eq!(a.db.total_changes(), changes, "no per-packet zero-run SQL");
        a.flush().unwrap();
        assert_eq!(a.read_blocks(&source.id).unwrap()[0].samples.len(), 6400);
        let changes = a.db.total_changes();
        a.flush().unwrap();
        assert_eq!(a.db.total_changes(), changes, "clean flush must not reopen a transaction");
        // The committed run keeps growing across commits.
        a.append(&source.id, 400_000_000, f, &[0.; 320], false).unwrap();
        a.flush().unwrap();
        assert_eq!(a.read_blocks(&source.id).unwrap()[0].samples.len(), 6720);
        a.gap(Some(&source.id), 420_000_000, "test gap").unwrap();
        a.append(&source.id, 420_000_000, f, &[0.; 320], false).unwrap();
        a.append(&source.id, 440_000_000, f, &[0.; 320], false).unwrap();
        let stereo = NativeFormat { sample_rate: 16000, channels: 2 };
        a.append(&source.id, 460_000_000, stereo, &[0.; 640], false).unwrap();
        a.append(&source.id, 480_000_000, stereo, &[0.; 640], false).unwrap();
        a.append(&source.id, 500_000_000, stereo, &[-0.; 640], false).unwrap();
        a.remove_source(&source.id, 520_000_000).unwrap();
        a.flush().unwrap();
        let rows = a.read_blocks(&source.id).unwrap();
        assert_eq!(rows.iter().map(|r| r.samples.len()).collect::<Vec<_>>(), [6720, 640, 1280, 640]);
        assert_eq!(rows[2].format, stereo);
        assert!(rows[3].samples.iter().all(|v| v.to_bits() == (-0_f32).to_bits()));
        let removed: Option<i64> = a.db.query_row("SELECT removed_ns FROM sources WHERE id=?1", [&source.id.0], |r| r.get(0)).unwrap();
        assert_eq!(removed, Some(520_000_000));
        assert_eq!(a.db.query_row("SELECT count(*) FROM gaps", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }
    #[test]
    fn native_round_trip_preserves_quiet_bits_and_zero_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Archive::create(&session(&dir)).unwrap();
        let f = NativeFormat { sample_rate: 48000, channels: 2 };
        let s = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "test".into() }, f, 1);
        a.add_source(&s, 100).unwrap();
        let pcm = vec![1e-9, -0., 0.5, -0.2, f32::from_bits(1), -1.];
        a.append(&s.id, 100, f, &pcm, false).unwrap();
        a.append(&s.id, 1_000_000, f, &[0.; 960], false).unwrap();
        a.append(&s.id, 11_000_000, f, &[0.; 960], false).unwrap();
        a.append(&s.id, 30_000_000, f, &[0.; 960], true).unwrap();
        a.flush().unwrap();
        let b = a.read_blocks(&s.id).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), pcm.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        assert_eq!(b[1].samples.len(), 1920);
        assert!(b[2].discontinuity);
        let p = a.path().to_owned();
        drop(a);
        let reopened = Archive::open_read_only(&p).unwrap();
        let persisted = reopened.read_blocks(&s.id).unwrap();
        assert_eq!(persisted.len(), 3);
        assert_eq!(persisted[0].host_ns, 100);
        assert_eq!(persisted[0].format, f);
        assert_eq!(persisted[0].samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), pcm.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        assert_eq!(persisted[1].samples.len(), 1920);
        assert!(persisted[2].discontinuity);
        assert_eq!(reopened.db.query_row("SELECT count(*) FROM sources", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        // No per-launch folder: the session is a row in the store.
        assert!(!p.exists());
        assert!(dir.path().join(store::INDEX).is_file());
    }
    #[test]
    fn a_collected_hot_object_is_rewritten_when_identical_audio_returns() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = Archive::create(&session(&dir)).unwrap();
        let f = NativeFormat { sample_rate: 16000, channels: 1 };
        let s = SourceInfo::new(SourceKind::InputDevice { name: "Synthetic".into(), uid: "repeat".into() }, f, 1);
        a.add_source(&s, 0).unwrap();
        let pcm: Vec<f32> = (0..16000).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        a.append(&s.id, 0, f, &pcm, false).unwrap();
        a.flush().unwrap();
        // The compactor publishes it cold and collects the hot blob.
        a.db.execute("UPDATE blocks SET wall_ns=1", []).unwrap();
        let now = heimdall_archive::wall_ns() + heimdall_archive::DAY_NS;
        assert_eq!(heimdall_archive::compact::compact_audio(dir.path(), now, 32000, 1).unwrap().audio_groups, 1);
        let id = audio::native_id(16000, 1, &pcm);
        assert!(!store::has(dir.path(), store::HOT, &store::audio_key(&id)).unwrap());
        // The same bits again (a replayed notification sound): a fresh hot object, readable.
        a.append(&s.id, 2_000_000_000, f, &pcm, false).unwrap();
        a.flush().unwrap();
        assert!(store::has(dir.path(), store::HOT, &store::audio_key(&id)).unwrap());
        let rows = a.read_blocks(&s.id).unwrap();
        assert_eq!(rows[1].samples, pcm);
    }
}
