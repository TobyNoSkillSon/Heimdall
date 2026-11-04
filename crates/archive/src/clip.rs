//! One time window of the archive as a movie: the screen recording of one display at its capture times, every
//! archived audio source on its own track, and the transcript as speaker-labelled subtitles (a soft track and a
//! sidecar `.srt`). "one tool … where you just choose the time … that outputs essentially a video …
//! all of the audio from different channels is in each individual track."
//!
//! Reading is read-only and safe beside the running app: one `SQLITE_OPEN_READ_ONLY` index connection (planning
//! runs in one read transaction, so a row moving into a cold chunk is seen exactly once), read-only pack
//! connections, and the shared lifetime lock for the whole build, so no blob the plan references can be collected
//! underneath it. Nothing is written to the store.
//!
//! Time: video samples, audio callbacks and transcript words share capture's host clock within a session. The clip
//! places each item at `offset + (host - host_from)` of its session, so audio and video stay in sync by
//! construction; a window that crosses app launches is laid out by wall time between sessions.
use crate::{audio, chunks, compact, hash, records::DICTIONARY, store, video, DAY_NS};
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(target_os = "macos")]
#[path = "clip_writer.rs"]
pub mod writer;

/// Longest clip one call builds; a longer window is split by the caller.
pub const MAX_CLIP_NS: u64 = 2 * 3_600_000_000_000;
/// One interleaving step: every track receives this much clip time before the next.
const WINDOW_NS: u64 = 1_000_000_000;
/// Consecutive callbacks within this much of each other's end are joined sample for sample, so host-clock jitter
/// never clicks; a larger difference (a gap, device clock drift) re-anchors the block at its own timestamp.
const SNAP_NS: u64 = 4_000_000;
/// Silence markers are bounded to one hour (`docs/archive.md`), so no block starts earlier than this before a window.
const LONGEST_BLOCK_NS: u64 = 3_600_000_000_000;
/// A frame whose coverage ends this long before the next one starts is followed by black, never held across a gap.
const VIDEO_GAP_NS: u64 = 250_000_000;
const NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    Hevc,
}
#[derive(Clone, Debug)]
pub struct Options {
    /// Unix wall-clock nanoseconds, start inclusive, end exclusive.
    pub from_ns: u64,
    pub to_ns: u64,
    /// Display ID; default: the display with the most recorded changes in the window.
    pub display: Option<u32>,
    /// Output width in pixels (height keeps the aspect ratio); default: the recorded width.
    pub width: Option<u32>,
    /// Constant output frame rate; default: every archived frame at its own capture time.
    pub fps: Option<u32>,
    pub codec: VideoCodec,
    /// Also give sources that were exact digital silence for the whole window a track.
    pub include_silent: bool,
    /// Embed the transcript as a soft subtitle track (the `.srt` sidecar is always written when there is speech).
    pub subtitles: bool,
    /// Verification only: write each frame's time and each track's placed PCM here.
    pub reference_dir: Option<PathBuf>,
}
impl Options {
    pub fn new(from_ns: u64, to_ns: u64) -> Self {
        Self {
            from_ns,
            to_ns,
            display: None,
            width: None,
            fps: None,
            codec: VideoCodec::H264,
            include_silent: false,
            subtitles: true,
            reference_dir: None,
        }
    }
}

// ---------------------------------------------------------------- read-only store access

/// The index read-only, plus read-only pack connections opened on first use. Never creates or writes anything.
pub(crate) struct Reader {
    root: PathBuf,
    pub(crate) db: Connection,
    packs: RefCell<HashMap<String, Connection>>,
}
impl Reader {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        ensure!(store::exists(root), "no archive store at {}", root.display());
        Ok(Self { root: root.to_path_buf(), db: store::read_index(root)?, packs: RefCell::new(HashMap::new()) })
    }
    fn blob(&self, pack: &str, key: &str) -> Result<Option<Vec<u8>>> {
        let mut packs = self.packs.borrow_mut();
        if !packs.contains_key(pack) {
            let path = store::pack_path(&self.root, pack)?;
            if !path.is_file() {
                return Ok(None);
            }
            let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(store::BUSY)?;
            packs.insert(pack.to_owned(), db);
        }
        let data = packs[pack].prepare_cached("SELECT data FROM blobs WHERE key=?1")?.query_row([key], |r| r.get(0)).optional()?;
        Ok(data)
    }
    fn checked(&self, pack: &str, key: &str, sha: &str) -> Result<Vec<u8>> {
        let bytes = self.blob(pack, key)?.with_context(|| format!("archive blob {pack}/{key} missing"))?;
        ensure!(hash(&bytes) == sha, "archive blob {pack}/{key} checksum mismatch");
        Ok(bytes)
    }
    fn chunk(&self, id: i64) -> Result<chunks::Tables> {
        let (pack, raw_bytes, sha): (String, i64, String) = self
            .db
            .prepare_cached("SELECT pack,raw_bytes,sha256 FROM main.cold_chunks WHERE id=?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        ensure!((0..=128 << 20).contains(&raw_bytes), "chunk bound");
        let data = self.blob(&pack, &store::chunk_key(&sha))?.with_context(|| format!("chunk {id} missing from pack {pack}"))?;
        let raw = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?.decompress(&data, raw_bytes as usize)?;
        ensure!(hash(&raw) == sha, "chunk checksum");
        chunks::decode(&raw)
    }
    /// One audio object, decoded (hot native float, or cold FLAC/Opus mono 16 kHz).
    fn object(&self, id: &str) -> Result<(audio::Pcm, String)> {
        let (rate, channels, frames, codec, pack, digest): (u32, u16, i64, String, String, String) = self
            .db
            .prepare_cached("SELECT rate,channels,frames,codec,pack,stored_sha256 FROM main.audio_objects WHERE id=?1")?
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .with_context(|| format!("audio object {id} is not catalogued"))?;
        let bytes = self.checked(&pack, &store::audio_key(id), &digest)?;
        Ok((audio::decode_object(rate, channels, frames as usize, &codec, &bytes)?, codec))
    }
}

// ---------------------------------------------------------------- time

/// The part of the window inside one app launch, on that session's host clock.
#[derive(Clone, Debug, Serialize)]
pub struct Piece {
    pub session: i64,
    pub name: String,
    pub host_from: u64,
    pub host_to: u64,
    /// Clip time of `host_from`.
    pub offset_ns: u64,
}
impl Piece {
    fn clip(&self, host: u64) -> i64 {
        self.offset_ns as i64 + (host as i64 - self.host_from as i64)
    }
}
/// Within one launch the host clock stops while the Mac sleeps, so wall − host jumps at each wake. Anchors read
/// wall and host together: capture segments (precise) and video epochs (one a minute, about 0.3 s coarse, and also
/// started after a wake). Returns (host where the epoch starts, wall − host) per stretch of unchanged offset.
fn clock_epochs(db: &Connection, session: i64) -> Result<Vec<(u64, i64, bool)>> {
    let mut anchors: Vec<(i64, i64, bool)> = db
        .prepare_cached("SELECT host_origin_ns,wall_unix_ns-host_origin_ns,1 FROM segments WHERE session=?1 UNION ALL SELECT start_ns,wall_ns-start_ns,0 FROM video_segments WHERE session=?1")?
        .query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    anchors.sort_by_key(|a| (a.0, !a.2));
    let mut out: Vec<(u64, i64, bool)> = vec![];
    for (host, offset, precise) in anchors {
        match out.last_mut() {
            Some(last) if (offset - last.1).abs() <= 2 * NS as i64 => {
                if precise && !last.2 {
                    (last.1, last.2) = (offset, true);
                }
            }
            _ => out.push((host.max(0) as u64, offset, precise)),
        }
    }
    if let Some(first) = out.first_mut() {
        first.0 = 0;
    }
    Ok(out)
}
fn pieces(db: &Connection, from: u64, to: u64) -> Result<Vec<Piece>> {
    let sessions: Vec<(i64, String, i64, Option<i64>)> = db
        .prepare("SELECT id,name,started_wall_ns,complete_wall_ns FROM sessions ORDER BY started_wall_ns,id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = vec![];
    for (i, (id, name, start, complete)) in sessions.iter().enumerate() {
        // A launch that crashed never completed: it ended no later than the next launch started.
        let next = sessions.get(i + 1).map(|s| s.2);
        let end = match (complete, next) {
            (Some(c), Some(n)) => (*c).min(n),
            (Some(c), None) => *c,
            (None, Some(n)) => n,
            (None, None) => i64::MAX,
        };
        let (a, b) = (from.max((*start).max(0) as u64), to.min(end.max(0) as u64));
        if b <= a {
            continue;
        }
        let epochs = clock_epochs(db, *id)?;
        for (k, &(h0, offset, _)) in epochs.iter().enumerate() {
            let wall = |h: u64| (h as i128 + offset as i128).clamp(0, u64::MAX as i128) as u64;
            let w0 = wall(h0);
            let w1 = epochs.get(k + 1).map_or(u64::MAX, |e| wall(e.0));
            let (wa, wb) = (a.max(w0), b.min(w1));
            if wb <= wa {
                continue;
            }
            let host = |w: u64| (w as i128 - offset as i128).max(0) as u64;
            out.push(Piece { session: *id, name: name.clone(), host_from: host(wa), host_to: host(wb), offset_ns: wa - from });
        }
    }
    out.retain(|p| p.host_to > p.host_from);
    out.sort_by_key(|p| p.offset_ns);
    Ok(out)
}

// ---------------------------------------------------------------- video plan

#[derive(Clone, Debug)]
struct SampleRef {
    host_ns: u64,
    end_ns: u64,
    key: bool,
    sha: String,
    pack: String,
}
#[derive(Debug)]
struct Epoch {
    segment: video::Segment,
    samples: Vec<SampleRef>,
}
/// One output frame: an archived sample (epoch, sample index) or black.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shot {
    Sample(usize, usize),
    Black,
}
#[derive(Debug)]
struct VideoPlan {
    display: u32,
    epochs: Vec<Epoch>,
    /// (clip ns, what to show, capture host ns of the sample) in presentation order.
    frames: Vec<(u64, Shot, Option<u64>)>,
    source_size: (u32, u32),
    size: (u32, u32),
    covered_ns: u64,
}
fn displays(db: &Connection, pieces: &[Piece]) -> Result<Vec<(u32, i64)>> {
    let mut counts = BTreeMap::<u32, i64>::new();
    for p in pieces {
        let mut stmt = db.prepare_cached("SELECT s.display,count(v.host_ns) FROM video_segments s LEFT JOIN video_samples v ON v.segment=s.id AND v.host_ns<?3 AND v.end_ns>?2 WHERE s.session=?1 AND s.end_ns>?2 AND s.start_ns<?3 GROUP BY s.display")?;
        for row in stmt.query_map(params![p.session, p.host_from as i64, p.host_to as i64], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?)))? {
            let (d, n) = row?;
            *counts.entry(d).or_default() += n;
        }
    }
    let mut list: Vec<_> = counts.into_iter().collect();
    list.sort_by_key(|(d, n)| (-n, *d));
    Ok(list)
}
fn plan_video(db: &Connection, pieces: &[Piece], options: &Options, duration: u64) -> Result<(VideoPlan, Vec<String>)> {
    let mut notes = vec![];
    let available = displays(db, pieces)?;
    let display = match options.display {
        Some(d) => {
            if !available.iter().any(|(x, _)| *x == d) {
                notes.push(format!("display {d} has no recording in this window"));
            }
            d
        }
        None => available.first().map(|(d, _)| *d).unwrap_or(1),
    };
    if available.len() > 1 {
        let others: Vec<String> = available.iter().filter(|(d, _)| *d != display).map(|(d, n)| format!("display {d} ({n} frames)")).collect();
        notes.push(format!("also recorded: {}", others.join(", ")));
    }
    let mut epochs = vec![];
    let mut frames: Vec<(u64, Shot, Option<u64>, u64)> = vec![];
    for p in pieces {
        let segments = db
            .prepare_cached("SELECT id,session,geometry,format,start_ns,end_ns,wall_ns,fps,tier,closed FROM video_segments WHERE session=?1 AND display=?2 AND end_ns>?3 AND start_ns<?4 ORDER BY start_ns,id")?
            .query_map(params![p.session, display, p.host_from as i64, p.host_to as i64], video::row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for segment in segments {
            let samples: Vec<SampleRef> = db
                .prepare_cached("SELECT host_ns,end_ns,keyframe,sha256,pack FROM video_samples WHERE segment=?1 AND host_ns<?2 ORDER BY host_ns")?
                .query_map(params![segment.id, p.host_to as i64], |r| {
                    Ok(SampleRef { host_ns: r.get::<_, i64>(0)? as u64, end_ns: r.get::<_, i64>(1)? as u64, key: r.get(2)?, sha: r.get(3)?, pack: r.get(4)? })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let e = epochs.len();
            for (i, s) in samples.iter().enumerate() {
                let a = s.host_ns.max(p.host_from);
                let b = s.end_ns.min(p.host_to);
                if b <= a {
                    continue;
                }
                let (t, end) = (p.clip(a).max(0) as u64, p.clip(b).max(0) as u64);
                // Rotation can overlap the outgoing epoch's tail by a frame: presentation times only move forward.
                if frames.last().is_some_and(|f| f.0 >= t) {
                    continue;
                }
                if let Some(last) = frames.last_mut() {
                    last.3 = last.3.min(t);
                }
                frames.push((t, Shot::Sample(e, i), Some(s.host_ns), end));
            }
            epochs.push(Epoch { segment, samples });
        }
    }
    let covered_ns = frames.iter().map(|f| f.3 - f.0).sum();
    // Black where nothing was recorded: before the first frame, across gaps and after the last.
    let mut out = vec![];
    let mut covered_to = 0;
    for (t, shot, host, end) in frames {
        if t > covered_to + if out.is_empty() { 0 } else { VIDEO_GAP_NS } {
            out.push((covered_to, Shot::Black, None));
        }
        out.push((t, shot, host));
        covered_to = end;
    }
    if out.is_empty() || covered_to + VIDEO_GAP_NS < duration {
        out.push((covered_to.min(duration.saturating_sub(1)), Shot::Black, None));
    }
    out.dedup_by(|b, a| a.0 == b.0);
    if covered_ns == 0 {
        notes.push(format!("no screen recording of display {display} in this window: the video is black"));
    }
    let source_size = epochs
        .iter()
        .find(|e| !e.samples.is_empty())
        .map(|e| (e.segment.format.width, e.segment.format.height))
        .unwrap_or((1280, 720));
    let size = match options.width {
        Some(w) => {
            let w = (w.clamp(64, 8192) / 2) * 2;
            let h = ((w as u64 * source_size.1 as u64 / source_size.0 as u64) as u32 / 2 * 2).max(2);
            (w, h)
        }
        None => (source_size.0 / 2 * 2, source_size.1 / 2 * 2),
    };
    if options.codec == VideoCodec::H264 {
        ensure!(size.0 <= 4096 && size.1 <= 4096, "H.264 is limited to 4096 pixels a side; pass a smaller width or HEVC");
    }
    let frames = match options.fps {
        None => out,
        Some(fps) => {
            ensure!((1..=60).contains(&fps), "fps must be 1-60");
            let mut ticks = vec![];
            let mut j = 0;
            let mut k = 0u64;
            loop {
                let t = k * NS / fps as u64;
                if t >= duration {
                    break;
                }
                while j + 1 < out.len() && out[j + 1].0 <= t {
                    j += 1;
                }
                ticks.push((t, out[j].1, out[j].2));
                k += 1;
            }
            ticks
        }
    };
    Ok((VideoPlan { display, epochs, frames, source_size, size, covered_ns }, notes))
}

// ---------------------------------------------------------------- audio plan

#[derive(Clone, Debug)]
struct Block {
    piece: usize,
    host_ns: u64,
    rate: u32,
    channels: u16,
    frames: u64,
    /// Interned: a one-second hot object backs about a hundred 10 ms callbacks, and a two-hour window holds
    /// hundreds of thousands of callbacks per source.
    codec: Arc<str>,
    object: Option<Arc<str>>,
    offset: u64,
    gain: f32,
}
#[derive(Default)]
struct Interner(HashMap<String, Arc<str>>);
impl Interner {
    fn get(&mut self, s: String) -> Arc<str> {
        self.0.entry(s).or_insert_with_key(|k| Arc::from(k.as_str())).clone()
    }
}
impl Block {
    fn end_ns(&self) -> u64 {
        self.host_ns + self.frames * NS / self.rate as u64
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackReport {
    /// 1-based audio track number in the file.
    pub track: usize,
    pub source: String,
    pub label: String,
    pub kind: String,
    pub rate: u32,
    pub channels: u16,
    /// Clip time covered by recorded (non-silent) callbacks.
    pub audio_ns: u64,
    /// How the archive holds this window: native lossless (hot) or cold FLAC/Opus seconds.
    pub stored: BTreeMap<String, f64>,
    /// Largest difference between where a callback was placed and its own capture timestamp.
    pub max_offset_us: u64,
}
struct Track {
    report: TrackReport,
    blocks: Vec<Block>,
}
const CALL_APPS: &[&str] = &[
    "com.hnc.Discord", "us.zoom.xos", "com.microsoft.teams", "com.microsoft.teams2", "com.apple.FaceTime",
    "com.tinyspeck.slackmacgap", "com.skype.skype", "net.whatsapp.WhatsApp", "ru.keepcoder.Telegram",
    "com.cisco.webexmeetingsapp", "com.google.Chrome.app.kjgfgldnnfoeklkmfkjfagphfepbbdan",
];
/// (kind, label) of one source from its capture metadata.
pub fn describe_source(id: &str, metadata: &str) -> (String, String) {
    let v: Value = serde_json::from_str(metadata).unwrap_or(Value::Null);
    let kind = &v["kind"];
    if let Some(device) = kind.get("InputDevice") {
        return ("mic".into(), format!("Mic: {}", device["name"].as_str().unwrap_or("input")));
    }
    if let Some(process) = kind.get("Process") {
        let name = process["name"].as_str().unwrap_or("app");
        let owner = process["owner"]["name"].as_str().unwrap_or(name);
        let bundle = process["owner"]["bundle_id"].as_str().or(process["bundle_id"].as_str()).unwrap_or("");
        let who = if name.starts_with(owner) { name.to_owned() } else { format!("{owner} ({name})") };
        if bundle == "dev.heimdall.app" {
            return ("heimdall".into(), "Heimdall".into());
        }
        if CALL_APPS.iter().any(|b| bundle.starts_with(b)) {
            return ("call".into(), format!("Call: {who}"));
        }
        return ("app".into(), format!("App: {who}"));
    }
    let label = v["label"].as_str().unwrap_or(id);
    ("audio".into(), label.to_owned())
}
fn block_from_row(piece: usize, cols: &[String], row: &[rusqlite::types::Value], strings: &mut Interner) -> Option<(i64, String, Block)> {
    use rusqlite::types::Value as V;
    let get = |name: &str| cols.iter().position(|c| c == name).map(|i| &row[i]);
    let int = |name: &str| match get(name) {
        Some(V::Integer(n)) => Some(*n),
        _ => None,
    };
    let text = |name: &str| match get(name) {
        Some(V::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let block = Block {
        piece,
        host_ns: int("host_ns")? as u64,
        rate: int("rate")? as u32,
        channels: int("channels")? as u16,
        frames: int("frames")? as u64,
        codec: strings.get(text("codec")?),
        object: text("object_id").map(|o| strings.get(o)),
        offset: int("offset_frames").unwrap_or(0) as u64,
        gain: match get("gain") {
            Some(V::Real(g)) => *g as f32,
            Some(V::Integer(g)) => *g as f32,
            _ => 1.,
        },
    };
    let source = text("source_id")?;
    (block.rate > 0).then_some((int("id").unwrap_or(0), source, block))
}
/// Every callback of every source overlapping each piece: live rows plus rows already packed into cold chunks.
fn plan_audio(db: &Connection, reader: &Reader, pieces: &[Piece]) -> Result<BTreeMap<String, (String, Vec<Block>)>> {
    let mut sources = BTreeMap::<String, (String, BTreeMap<(usize, u64, i64), Block>)>::new();
    let mut strings = Interner::default();
    for (pi, p) in pieces.iter().enumerate() {
        let (from, to) = (p.host_from as i64, p.host_to as i64);
        let earliest = (p.host_from.saturating_sub(LONGEST_BLOCK_NS)) as i64;
        let listed: Vec<(String, String)> = db
            .prepare_cached("SELECT s.id,max(s.metadata_json) FROM sources s JOIN segments g ON g.id=s.segment WHERE g.session=?1 GROUP BY s.id")?
            .query_map([p.session], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut live = db.prepare_cached("SELECT b.id,b.host_ns,b.rate,b.channels,b.frames,b.codec,b.object_id,b.offset_frames,b.gain FROM segments g JOIN blocks b ON b.segment=g.id WHERE g.session=?1 AND b.source_id=?2 AND b.host_ns>=?3 AND b.host_ns<?4 AND b.host_ns+b.frames*1000000000/b.rate>?5")?;
        for (source, metadata) in &listed {
            let rows = live.query_map(params![p.session, source, earliest, to, from], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, u32>(2)?,
                    r.get::<_, u16>(3)?,
                    r.get::<_, i64>(4)? as u64,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, i64>(7)? as u64,
                    r.get::<_, f64>(8)? as f32,
                ))
            })?;
            for row in rows {
                let (id, host_ns, rate, channels, frames, codec, object, offset, gain) = row?;
                let b = Block { piece: pi, host_ns, rate, channels, frames, codec: strings.get(codec), object: object.map(|o| strings.get(o)), offset, gain };
                if b.rate == 0 {
                    continue;
                }
                let entry = sources.entry(source.clone()).or_insert_with(|| (metadata.clone(), BTreeMap::new()));
                entry.1.insert((pi, b.host_ns, id), b);
            }
        }
        // Cold rows: chunks whose catalogued span overlaps, and any chunk not catalogued yet.
        let ids: Vec<i64> = db
            .prepare_cached("SELECT DISTINCT c.id FROM cold_chunks c JOIN segments g ON g.id=c.scope WHERE c.kind='blocks' AND g.session=?1 AND (EXISTS(SELECT 1 FROM cold_capture_spans r WHERE r.chunk_id=c.id AND r.start_ns<?3 AND r.end_ns>?2) OR NOT EXISTS(SELECT 1 FROM cold_capture_indexed i WHERE i.chunk_id=c.id)) ORDER BY c.id")?
            .query_map(params![p.session, from, to], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            for (name, cols, rows) in reader.chunk(id)? {
                if name != "blocks" {
                    continue;
                }
                for row in rows {
                    let Some((id, source, b)) = block_from_row(pi, &cols, &row, &mut strings) else { continue };
                    if b.host_ns >= p.host_to || b.end_ns() <= p.host_from {
                        continue;
                    }
                    let metadata = listed.iter().find(|(s, _)| *s == source).map(|(_, m)| m.clone()).unwrap_or_default();
                    let entry = sources.entry(source).or_insert_with(|| (metadata, BTreeMap::new()));
                    entry.1.insert((pi, b.host_ns, id), b);
                }
            }
        }
    }
    Ok(sources.into_iter().map(|(s, (m, blocks))| (s, (m, blocks.into_values().collect()))).collect())
}

/// Places one track's callbacks on the clip's sample grid, one window at a time.
struct Placer {
    rate: u32,
    channels: u16,
    next: usize,
    /// Absolute output frame of `staged[0]`.
    staged_from: u64,
    staged: Vec<f32>,
    /// Output frame where the previous callback ended (continuity anchor).
    cursor: Option<i64>,
    max_offset: u64,
    /// The last decoded object and how the archive stores it.
    cache: Option<(Arc<str>, audio::Pcm, String)>,
    /// Audio the archive could not return; that stretch stays silent and the report says so.
    unreadable: Vec<String>,
}
impl Placer {
    fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels, next: 0, staged_from: 0, staged: vec![], cursor: None, max_offset: 0, cache: None, unreadable: vec![] }
    }
    fn frame_at(&self, clip_ns: i64) -> i64 {
        (clip_ns as i128 * self.rate as i128).div_euclid(NS as i128) as i64
    }
    /// Output frames [j0, j1) of a callback on its own native timeline: rounded at both ends, so consecutive
    /// callbacks of one object abut exactly at any pair of rates (44.1 kHz from 16 kHz never drops a frame).
    fn span(&self, b: &Block) -> (u64, u64) {
        let (r, br) = (self.rate as u64, b.rate as u64);
        ((b.offset * r + br / 2) / br, ((b.offset + b.frames) * r + br / 2) / br)
    }
    /// The block's samples, converted to this track's channels and rate (`None`: exact silence), and how the archive
    /// stores them.
    fn convert(&mut self, reader: &Reader, b: &Block) -> Result<(Option<Vec<f32>>, String)> {
        if &*b.codec == "zero" {
            return Ok((None, "exact silence".into()));
        }
        let object = b.object.clone().context("callback has no audio object")?;
        if self.cache.as_ref().is_none_or(|(id, _, _)| *id != object) {
            let (pcm, codec) = reader.object(&object)?;
            let tier = match codec.as_str() {
                "f32-zstd-planar-v1" | "f32-zstd" => format!("native {} Hz lossless", pcm.rate),
                "flac16" => "FLAC 16 kHz mono (speech)".to_owned(),
                "opus-har1" => "Opus 12 kb/s 16 kHz mono (non-speech)".to_owned(),
                other => other.to_owned(),
            };
            self.cache = Some((object, pcm, tier));
        }
        let (j0, j1) = self.span(b);
        let (_, p, tier) = self.cache.as_ref().unwrap();
        let pc = p.channels as usize;
        let n = p.samples.len() / pc;
        ensure!(n > 0 && ((b.offset + b.frames) * p.rate as u64).div_ceil(b.rate as u64) <= n as u64, "callback exceeds its audio object");
        let oc = self.channels as usize;
        // Frame `i` of the whole object (clamped), so interpolation runs on across callback edges.
        let frame = |i: usize, c: usize| -> f32 {
            let i = i.min(n - 1);
            let f = &p.samples[i * pc..(i + 1) * pc];
            let v = if pc == oc {
                f[c]
            } else if pc == 1 {
                f[0]
            } else if oc == 1 {
                f.iter().sum::<f32>() / pc as f32
            } else {
                // More than two source channels: even ones left, odd ones right.
                let side: Vec<f32> = f.iter().skip(c).step_by(2).copied().collect();
                side.iter().sum::<f32>() / side.len().max(1) as f32
            };
            v * b.gain
        };
        let mut out = Vec::with_capacity((j1 - j0) as usize * oc);
        for j in j0..j1 {
            if p.rate == self.rate {
                for c in 0..oc {
                    out.push(frame(j as usize, c));
                }
            } else {
                // Linear interpolation in object coordinates: cold tiers are 16 kHz mono, some sources 44.1 kHz.
                let x = j as f64 * p.rate as f64 / self.rate as f64;
                let i = x as usize;
                let w = (x - i as f64) as f32;
                for c in 0..oc {
                    out.push(frame(i, c) * (1. - w) + frame(i + 1, c) * w);
                }
            }
        }
        Ok((Some(out), tier.clone()))
    }
    /// Output frames [from, to) of the track, interleaved.
    fn render(&mut self, reader: &Reader, pieces: &[Piece], blocks: &[Block], tiers: &mut BTreeMap<String, f64>, from: u64, to: u64, covered: &mut u64) -> Result<Vec<f32>> {
        let oc = self.channels as usize;
        while self.next < blocks.len() {
            let b = &blocks[self.next];
            let p = &pieces[b.piece];
            let wanted = self.frame_at(p.clip(b.host_ns));
            if wanted >= to as i64 {
                break;
            }
            self.next += 1;
            let snap = (SNAP_NS * self.rate as u64 / NS) as i64;
            let at = match self.cursor {
                Some(c) if (wanted - c).abs() <= snap => c,
                _ => wanted,
            };
            self.max_offset = self.max_offset.max(((at - wanted).unsigned_abs()) * 1_000_000 / self.rate as u64);
            // Clip to the piece, so a callback never spills into another launch's time.
            let piece_end = self.frame_at(p.clip(p.host_to));
            let piece_start = self.frame_at(p.offset_ns as i64);
            let (j0, j1) = self.span(b);
            let len = (j1 - j0) as i64;
            self.cursor = Some(at + len);
            let lo = at.max(piece_start).max(self.staged_from as i64);
            let hi = (at + len).min(piece_end);
            if hi <= lo {
                continue;
            }
            let (samples, tier) = match self.convert(reader, b) {
                Ok(c) => c,
                Err(e) => {
                    // One damaged object costs its own second of sound, not the clip.
                    if self.unreadable.len() < 3 {
                        self.unreadable.push(format!("{e:#}"));
                    }
                    (None, "unreadable".to_owned())
                }
            };
            // Only the part inside the clip counts.
            *tiers.entry(tier).or_default() += (hi - lo) as f64 / self.rate as f64;
            let Some(samples) = samples else { continue };
            *covered += ((hi - lo) as u64) * NS / self.rate as u64;
            let need = (hi as u64 - self.staged_from) as usize * oc;
            if self.staged.len() < need {
                self.staged.resize(need, 0.);
            }
            let dst = (lo as u64 - self.staged_from) as usize * oc;
            let src = (lo - at) as usize * oc;
            let n = (hi - lo) as usize * oc;
            self.staged[dst..dst + n].copy_from_slice(&samples[src..src + n]);
        }
        ensure!(from == self.staged_from, "audio windows must be contiguous");
        let n = (to - from) as usize * oc;
        if self.staged.len() < n {
            self.staged.resize(n, 0.);
        }
        let rest = self.staged.split_off(n);
        let out = std::mem::replace(&mut self.staged, rest);
        self.staged_from = to;
        Ok(out)
    }
}

// ---------------------------------------------------------------- transcript

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cue {
    pub start_ns: u64,
    pub end_ns: u64,
    pub text: String,
}
/// Subtitle lines of at most this long, or this many characters.
const CUE_NS: u64 = 5_500_000_000;
const CUE_CHARS: usize = 84;
const CUE_HOLD_NS: u64 = 400_000_000;
const CUE_MIN_NS: u64 = 800_000_000;
/// Speaker label of an utterance: the named person, or the source when only "audio" is known.
pub fn speaker_label(speaker: Option<&str>, source: Option<&str>) -> String {
    match speaker.map(str::trim) {
        Some(s) if !s.is_empty() && s != "audio" => s.to_owned(),
        _ => source.filter(|s| !s.is_empty()).unwrap_or("Audio").to_owned(),
    }
}
/// One utterance's words → short timed lines (clip ns). Words carry host times on the utterance's source clock.
pub fn utterance_cues(label: &str, text: &str, words: &[(String, u64, u64)], start: u64, end: u64, clip: &dyn Fn(u64) -> i64) -> Vec<Cue> {
    let mut out = vec![];
    let push = |out: &mut Vec<Cue>, a: u64, b: u64, line: &[&str]| {
        let line = line.join(" ");
        if !line.trim().is_empty() {
            let (a, b) = (clip(a), clip(b.max(a + CUE_MIN_NS)));
            if b > 0 {
                out.push(Cue { start_ns: a.max(0) as u64, end_ns: b as u64, text: format!("{label}: {}", line.trim()) });
            }
        }
    };
    if words.is_empty() {
        // No timed words: spread the text evenly over the utterance.
        let tokens: Vec<&str> = text.split_whitespace().collect();
        if tokens.is_empty() {
            return out;
        }
        let span = end.saturating_sub(start).max(CUE_MIN_NS);
        let parts = (span.div_ceil(CUE_NS) as usize).max(text.len().div_ceil(CUE_CHARS)).clamp(1, tokens.len());
        for (i, chunk) in tokens.chunks(tokens.len().div_ceil(parts)).enumerate() {
            let a = start + span * i as u64 / parts as u64;
            push(&mut out, a, a + span / parts as u64, chunk);
        }
        return out;
    }
    let mut line: Vec<&str> = vec![];
    let (mut a, mut b) = (words[0].1, words[0].2);
    for (i, (w, ws, we)) in words.iter().enumerate() {
        let chars = line.iter().map(|x| x.len() + 1).sum::<usize>() + w.len();
        let gap = ws.saturating_sub(b);
        if !line.is_empty() && (we.saturating_sub(a) > CUE_NS || chars > CUE_CHARS || gap > 1_500_000_000) {
            let hold = (b + CUE_HOLD_NS).min(*ws);
            push(&mut out, a, hold.max(b), &line);
            line.clear();
            a = *ws;
        }
        line.push(w.as_str());
        b = (*we).max(b);
        let sentence = w.ends_with(['.', '?', '!']);
        if sentence && b.saturating_sub(a) >= 2 * NS && i + 1 < words.len() {
            let hold = (b + CUE_HOLD_NS).min(words[i + 1].1);
            push(&mut out, a, hold.max(b), &line);
            line.clear();
            a = words[i + 1].1;
        }
    }
    push(&mut out, a, b + CUE_HOLD_NS, &line);
    out
}
/// Overlapping lines (two people at once) become one timeline of non-overlapping cues, one line per speaker.
pub fn merge_cues(mut cues: Vec<Cue>, duration: u64) -> Vec<Cue> {
    cues.retain(|c| c.start_ns < duration && c.end_ns > c.start_ns);
    for c in &mut cues {
        c.end_ns = c.end_ns.min(duration);
    }
    cues.sort_by_key(|c| (c.start_ns, c.end_ns));
    let mut edges: Vec<u64> = cues.iter().flat_map(|c| [c.start_ns, c.end_ns]).collect();
    edges.sort_unstable();
    edges.dedup();
    let mut out: Vec<Cue> = vec![];
    for w in edges.windows(2) {
        let (a, b) = (w[0], w[1]);
        let active: Vec<&str> = cues.iter().filter(|c| c.start_ns <= a && c.end_ns >= b).map(|c| c.text.as_str()).collect();
        if active.is_empty() {
            continue;
        }
        let text = active[active.len().saturating_sub(3)..].join("\n");
        match out.last_mut() {
            Some(last) if last.end_ns == a && last.text == text => last.end_ns = b,
            _ => out.push(Cue { start_ns: a, end_ns: b, text }),
        }
    }
    out
}
pub fn srt(cues: &[Cue]) -> String {
    let stamp = |ns: u64| {
        let ms = ns / 1_000_000;
        format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
    };
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, stamp(c.start_ns), stamp(c.end_ns), c.text));
    }
    out
}
fn plan_cues(db: &Connection, pieces: &[Piece], duration: u64) -> Result<(Vec<Cue>, usize, Vec<String>)> {
    let mut cues = vec![];
    let mut utterances = 0;
    let mut speakers: Vec<String> = vec![];
    for p in pieces {
        let rows: Vec<(Option<String>, Option<String>, i64, i64, Option<String>, Option<String>)> = db
            .prepare_cached("SELECT speaker,source,start_ns,end_ns,text,words FROM utterances WHERE session=?1 AND start_ns<?3 AND end_ns>?2 ORDER BY start_ns")?
            .query_map(params![p.session, p.host_from as i64, p.host_to as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (speaker, source, start, end, text, words) in rows {
            let label = speaker_label(speaker.as_deref(), source.as_deref());
            let words: Vec<(String, u64, u64)> = words
                .and_then(|w| serde_json::from_str::<Value>(&w).ok())
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|w| Some((w["text"].as_str()?.trim().to_owned(), w["start_ns"].as_u64()?, w["end_ns"].as_u64()?)))
                .filter(|w| !w.0.is_empty() && w.2 > p.host_from && w.1 < p.host_to)
                .collect();
            let text = text.unwrap_or_default();
            if words.is_empty() && text.trim().is_empty() {
                continue;
            }
            let clip = |h: u64| p.clip(h.clamp(p.host_from, p.host_to));
            cues.extend(utterance_cues(&label, &text, &words, start.max(0) as u64, end.max(0) as u64, &clip));
            utterances += 1;
            if !speakers.contains(&label) {
                speakers.push(label);
            }
        }
    }
    Ok((merge_cues(cues, duration), utterances, speakers))
}

// ---------------------------------------------------------------- report

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoReport {
    pub display: u32,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
    pub codec: VideoCodec,
    pub frames: usize,
    /// Stretches with no recording, shown black.
    pub black_gaps: usize,
    /// Frame rates and tiers of the archived epochs used ("5 fps hot", "1 fps archive").
    pub archived: Vec<String>,
    pub covered_ns: u64,
    pub fps: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub path: PathBuf,
    pub srt: Option<PathBuf>,
    pub from_ns: u64,
    pub to_ns: u64,
    pub duration_ns: u64,
    pub bytes: u64,
    pub sessions: Vec<String>,
    pub video: VideoReport,
    pub audio: Vec<TrackReport>,
    /// Sources that were exact digital silence for the whole window (no track unless asked).
    pub silent: Vec<String>,
    pub cues: usize,
    pub utterances: usize,
    pub speakers: Vec<String>,
    pub subtitles_embedded: bool,
    pub notes: Vec<String>,
}
fn clock(ns: u64) -> String {
    let s = ns / NS;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}
impl Report {
    /// Plain lines for a person or an agent; no JSON.
    pub fn text(&self) -> String {
        let mut out = vec![format!("Clip: {} ({}, {:.1} MB)", self.path.display(), clock(self.duration_ns), self.bytes as f64 / 1e6)];
        let v = &self.video;
        let timing = match v.fps {
            Some(f) => format!("{f} fps constant"),
            None => "each archived frame at its capture time".to_owned(),
        };
        out.push(format!(
            "Video: display {}, {}x{} {}, {} frames, {timing}; archived as {}; {} of {} recorded{}.",
            v.display,
            v.width,
            v.height,
            if v.codec == VideoCodec::H264 { "H.264" } else { "HEVC" },
            v.frames,
            if v.archived.is_empty() { "nothing".to_owned() } else { v.archived.join(", ") },
            clock(v.covered_ns),
            clock(self.duration_ns),
            if v.black_gaps > 0 { format!(", black where nothing was recorded ({} gaps)", v.black_gaps) } else { String::new() }
        ));
        if self.audio.is_empty() {
            out.push("Audio: no audio was recorded in this window; the clip has video and subtitles only.".into());
        }
        for t in &self.audio {
            let stored: Vec<String> = t.stored.iter().map(|(k, s)| format!("{k} {}", clock((*s * 1e9) as u64))).collect();
            out.push(format!(
                "Audio track {}: {} ({} Hz {}), sound for {}; stored as {}; placed within {:.1} ms of its capture times.",
                t.track,
                t.label,
                t.rate,
                if t.channels == 1 { "mono" } else { "stereo" },
                clock(t.audio_ns),
                stored.join(", "),
                t.max_offset_us as f64 / 1000.
            ));
        }
        if !self.silent.is_empty() {
            out.push(format!("Left out, silent the whole window: {}.", self.silent.join(", ")));
        }
        out.push(match (&self.srt, self.cues) {
            (_, 0) => "Subtitles: nobody was transcribed in this window.".to_owned(),
            (Some(srt), n) => format!(
                "Subtitles: {n} cues from {} utterances ({}){}, sidecar {}.",
                self.utterances,
                self.speakers.join(", "),
                if self.subtitles_embedded { ", as a soft subtitle track" } else { "" },
                srt.display()
            ),
            (None, n) => format!("Subtitles: {n} cues."),
        });
        for n in &self.notes {
            out.push(format!("Note: {n}."));
        }
        out.join("\n")
    }
}

/// What the archive keeps, for the tool's answer and docs (`docs/archive-clip.md`).
pub const ARCHIVE_FACTS: &str = "The archive keeps each display's screen as HEVC at half native size, 5 fps, for about 24 hours, then re-encoded to at most 1 fps; every audio source as its own native float stream, lossless, for about 24 hours, then FLAC 16 kHz mono for speech and Opus 12 kb/s for the rest, never deleted; the transcript with word times.";

// ---------------------------------------------------------------- build

/// A muxer the clip is streamed into (the native AVAssetWriter, or a test sink).
pub trait Sink {
    /// One frame at clip time `t_ns`: tightly packed BGRA at the output size, or black.
    fn video(&mut self, t_ns: u64, bgra: Option<&[u8]>) -> Result<()>;
    /// Interleaved float PCM for audio track `track` (0-based) starting at output frame `frame`.
    fn audio(&mut self, track: usize, frame: u64, pcm: &[f32]) -> Result<()>;
    fn text(&mut self, start_ns: u64, end_ns: u64, text: &str) -> Result<()>;
    /// End of one interleaving window (clip time `end_ns`): hand queued media to the writer. Every `text` call comes
    /// before the first `video` call.
    fn window(&mut self, end_ns: u64) -> Result<()>;
}
/// The layout the sink must be created with.
#[derive(Clone, Debug)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub codec: VideoCodec,
    pub fps_hint: u32,
    pub video_title: String,
    /// (rate, channels, title) per audio track.
    pub audio: Vec<(u32, u16, String)>,
    pub subtitles: bool,
    pub duration_ns: u64,
}

pub struct Plan {
    reader: Reader,
    pieces: Vec<Piece>,
    video: VideoPlan,
    tracks: Vec<Track>,
    silent: Vec<String>,
    cues: Vec<Cue>,
    utterances: usize,
    speakers: Vec<String>,
    notes: Vec<String>,
    options: Options,
    duration: u64,
    _lock: compact::ReaderLock,
}
impl Plan {
    /// Reads everything the clip needs from one consistent snapshot of the index. Holds the shared lifetime lock
    /// until the plan is dropped, so referenced blobs stay put while the clip is written.
    pub fn new(root: &Path, options: &Options) -> Result<Self> {
        ensure!(options.to_ns > options.from_ns, "the end of the window must be after its start");
        let duration = options.to_ns - options.from_ns;
        ensure!(duration <= MAX_CLIP_NS, "a clip is at most two hours; split the window");
        // Checked first: the lock would create lock files in a folder that holds no archive.
        ensure!(store::exists(root), "no archive store at {}", root.display());
        let lock = compact::reader_lock(root)?;
        let reader = Reader::open(root)?;
        reader.db.execute_batch("BEGIN")?;
        let planned = (|| -> Result<_> {
            let db = &reader.db;
            let pieces = pieces(db, options.from_ns, options.to_ns)?;
            ensure!(!pieces.is_empty(), "the archive has nothing recorded in this window");
            let (video, mut notes) = plan_video(db, &pieces, options, duration)?;
            let sources = plan_audio(db, &reader, &pieces)?;
            let (cues, utterances, speakers) = plan_cues(db, &pieces, duration)?;
            Ok((pieces, video, sources, cues, utterances, speakers, std::mem::take(&mut notes)))
        })();
        reader.db.execute_batch("COMMIT")?;
        let (pieces, video, sources, cues, utterances, speakers, mut notes) = planned?;
        let mut tracks = vec![];
        let mut silent = vec![];
        let mut labels = HashMap::<String, usize>::new();
        for (source, (metadata, blocks)) in sources {
            let (kind, mut label) = describe_source(&source, &metadata);
            let n = labels.entry(label.clone()).or_default();
            *n += 1;
            if *n > 1 {
                label = format!("{label} #{n}");
            }
            let sound: Vec<&Block> = blocks.iter().filter(|b| &*b.codec != "zero").collect();
            if sound.is_empty() && !options.include_silent {
                silent.push(label);
                continue;
            }
            // The rate most of the window was captured at; at most two channels.
            let mut rates = BTreeMap::<u32, u64>::new();
            for b in &sound {
                *rates.entry(b.rate).or_default() += b.frames;
            }
            let rate = rates.into_iter().max_by_key(|(_, n)| *n).map_or(48_000, |(r, _)| r);
            // AAC takes these rates; anything else (96 kHz interfaces) is resampled to 48 kHz.
            let rate = if [8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000].contains(&rate) { rate } else { 48_000 };
            let channels = blocks.iter().map(|b| b.channels).max().unwrap_or(1).clamp(1, 2);
            tracks.push(Track {
                report: TrackReport { track: 0, source, label, kind, rate, channels, audio_ns: 0, stored: BTreeMap::new(), max_offset_us: 0 },
                blocks,
            });
        }
        let order = |k: &str| ["mic", "call", "app", "audio", "heimdall"].iter().position(|x| *x == k).unwrap_or(9);
        tracks.sort_by(|a, b| (order(&a.report.kind), &a.report.label).cmp(&(order(&b.report.kind), &b.report.label)));
        for (i, t) in tracks.iter_mut().enumerate() {
            t.report.track = i + 1;
        }
        if pieces.len() > 1 {
            notes.push(format!("the window spans {} app launches; time between them is laid out by wall clock", pieces.len()));
        }
        Ok(Self { reader, pieces, video, tracks, silent, cues, utterances, speakers, notes, options: options.clone(), duration, _lock: lock })
    }
    pub fn layout(&self) -> Layout {
        let fps_hint = self.options.fps.unwrap_or_else(|| self.video.epochs.iter().map(|e| e.segment.fps).max().unwrap_or(5));
        Layout {
            width: self.video.size.0,
            height: self.video.size.1,
            codec: self.options.codec,
            fps_hint,
            video_title: format!("Screen: display {}", self.video.display),
            audio: self.tracks.iter().map(|t| (t.report.rate, t.report.channels, t.report.label.clone())).collect(),
            subtitles: self.options.subtitles && !self.cues.is_empty(),
            duration_ns: self.duration,
        }
    }
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }
    /// Streams the clip into `sink`, one second at a time. `more` is asked between windows (cancellation).
    pub fn write(&mut self, sink: &mut dyn Sink, more: &mut dyn FnMut() -> bool) -> Result<Report> {
        let layout = self.layout();
        if layout.subtitles {
            for c in &self.cues {
                sink.text(c.start_ns, c.end_ns, &c.text)?;
            }
        }
        let mut frames = Frames::new(&self.reader, &self.video)?;
        let mut placers: Vec<Placer> = self.tracks.iter().map(|t| Placer::new(t.report.rate, t.report.channels)).collect();
        let mut reference: Option<(PathBuf, Vec<std::fs::File>, std::fs::File)> = match &self.options.reference_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let files = (0..self.tracks.len()).map(|i| std::fs::File::create(dir.join(format!("track-{}.f32", i + 1)))).collect::<std::io::Result<_>>()?;
                Some((dir.clone(), files, std::fs::File::create(dir.join("frames.txt"))?))
            }
            None => None,
        };
        let mut next_frame = 0;
        let mut t0 = 0;
        while t0 < self.duration {
            ensure!(more(), "clip cancelled");
            let t1 = (t0 + WINDOW_NS).min(self.duration);
            // Each window's sound first, then its frames: the writer interleaves by time and then never waits on audio
            // while frames queue up.
            for (i, (track, placer)) in self.tracks.iter_mut().zip(&mut placers).enumerate() {
                let (a, b) = ((t0 as u128 * track.report.rate as u128 / NS as u128) as u64, (t1 as u128 * track.report.rate as u128 / NS as u128) as u64);
                let pcm = placer.render(&self.reader, &self.pieces, &track.blocks, &mut track.report.stored, a, b, &mut track.report.audio_ns)?;
                if !pcm.is_empty() {
                    sink.audio(i, a, &pcm)?;
                }
                if let Some((_, files, _)) = reference.as_mut() {
                    use std::io::Write;
                    files[i].write_all(&audio::pcm_bytes(&pcm))?;
                }
            }
            while next_frame < self.video.frames.len() && self.video.frames[next_frame].0 < t1 {
                let (t, shot, host) = self.video.frames[next_frame];
                let pixels = frames.get(shot);
                sink.video(t, pixels)?;
                if let Some((_, _, f)) = reference.as_mut() {
                    use std::io::Write;
                    writeln!(f, "{t} {}", host.map_or("black".to_owned(), |h| h.to_string()))?;
                }
                next_frame += 1;
            }
            sink.window(t1)?;
            t0 = t1;
        }
        for (track, placer) in self.tracks.iter_mut().zip(&placers) {
            track.report.max_offset_us = placer.max_offset;
            track.report.stored.retain(|_, s| *s > 0.);
            if !placer.unreadable.is_empty() {
                self.notes.push(format!("parts of {} could not be read from the archive and are silent ({})", track.report.label, placer.unreadable.join("; ")));
            }
        }
        if !frames.unreadable.is_empty() {
            self.notes.push(format!("{} frames could not be decoded and are black ({})", frames.failed, frames.unreadable.join("; ")));
        }
        let mut archived: Vec<String> = vec![];
        for e in self.video.epochs.iter().filter(|e| !e.samples.is_empty()) {
            let label = format!("{}x{} {} fps {}", e.segment.format.width, e.segment.format.height, e.segment.fps, e.segment.tier);
            if !archived.contains(&label) {
                archived.push(label);
            }
        }
        if let Some((dir, _, _)) = &reference {
            std::fs::write(dir.join("cues.srt"), srt(&self.cues))?;
        }
        Ok(Report {
            path: PathBuf::new(),
            srt: None,
            from_ns: self.options.from_ns,
            to_ns: self.options.to_ns,
            duration_ns: self.duration,
            bytes: 0,
            sessions: self.pieces.iter().map(|p| p.name.clone()).collect(),
            video: VideoReport {
                display: self.video.display,
                width: self.video.size.0,
                height: self.video.size.1,
                source_width: self.video.source_size.0,
                source_height: self.video.source_size.1,
                codec: self.options.codec,
                frames: self.video.frames.len(),
                black_gaps: self.video.frames.iter().enumerate().filter(|(i, f)| f.1 == Shot::Black && (*i == 0 || self.video.frames[i - 1].1 != Shot::Black)).count(),
                archived,
                covered_ns: self.video.covered_ns,
                fps: self.options.fps,
            },
            audio: self.tracks.iter().map(|t| t.report.clone()).collect(),
            silent: self.silent.clone(),
            cues: self.cues.len(),
            utterances: self.utterances,
            speakers: self.speakers.clone(),
            subtitles_embedded: layout.subtitles,
            notes: self.notes.clone(),
        })
    }
}

/// Decodes the planned samples in order and hands out each requested frame at the output size.
struct Frames<'a> {
    reader: &'a Reader,
    plan: &'a VideoPlan,
    /// (epoch, last decoded sample index)
    at: Option<(usize, usize)>,
    #[cfg(target_os = "macos")]
    decoder: Option<video::native::Decoder>,
    last: Option<(Shot, Vec<u8>)>,
    black: Vec<u8>,
    /// An epoch whose decode failed: the rest of it is black (its later frames depend on the lost one).
    broken: Option<usize>,
    failed: usize,
    unreadable: Vec<String>,
}
impl<'a> Frames<'a> {
    fn new(reader: &'a Reader, plan: &'a VideoPlan) -> Result<Self> {
        let (w, h) = plan.size;
        let black = [0, 0, 0, 255].repeat(w as usize * h as usize);
        Ok(Self {
            reader,
            plan,
            at: None,
            #[cfg(target_os = "macos")]
            decoder: None,
            last: None,
            black,
            broken: None,
            failed: 0,
            unreadable: vec![],
        })
    }
    fn sample(&self, e: usize, i: usize) -> Result<video::Sample> {
        let s = &self.plan.epochs[e].samples[i];
        Ok(video::Sample { host_ns: s.host_ns, end_ns: s.end_ns, key: s.key, data: self.reader.checked(&s.pack, &video::key(&s.sha), &s.sha)? })
    }
    /// The frame for `shot` at the output size; `None` is black: no recording, or a frame the archive could not
    /// return (counted for the report, so one damaged sample costs its epoch, not the clip).
    fn get(&mut self, shot: Shot) -> Option<&[u8]> {
        let Shot::Sample(e, i) = shot else { return None };
        if self.last.as_ref().is_some_and(|(s, _)| *s == shot) {
            return Some(&self.last.as_ref().unwrap().1);
        }
        if self.broken == Some(e) {
            self.failed += 1;
            return None;
        }
        let epoch = &self.plan.epochs[e];
        let (sw, sh) = (epoch.segment.format.width, epoch.segment.format.height);
        let bgra = if epoch.segment.format.parameters.is_empty() {
            // Pre-HEVC epochs stored each frame as an image.
            self.sample(e, i).and_then(|s| Ok(image::load_from_memory(&s.data)?.to_rgba8().pixels().flat_map(|p| [p[2], p[1], p[0], 255]).collect()))
        } else {
            self.decode(e, i)
        };
        match bgra {
            Ok(bgra) => {
                let pixels = scale(&bgra, sw, sh, self.plan.size.0, self.plan.size.1, &self.black);
                self.last = Some((shot, pixels));
                Some(&self.last.as_ref().unwrap().1)
            }
            Err(error) => {
                if self.unreadable.len() < 3 {
                    self.unreadable.push(format!("{error:#}"));
                }
                (self.broken, self.at) = (Some(e), None);
                #[cfg(target_os = "macos")]
                {
                    self.decoder = None;
                }
                self.failed += 1;
                None
            }
        }
    }
    #[cfg(target_os = "macos")]
    fn decode(&mut self, e: usize, i: usize) -> Result<Vec<u8>> {
        // A new epoch, or a request behind the decoder: restart from the epoch's first keyframe.
        let start = match self.at {
            Some((ae, ai)) if ae == e && ai < i => ai + 1,
            _ => {
                self.decoder = Some(video::native::Decoder::new(&self.plan.epochs[e].segment.format)?);
                0
            }
        };
        let decoder = self.decoder.as_mut().unwrap();
        for k in start..i {
            let s = &self.plan.epochs[e].samples[k];
            let sample = video::Sample { host_ns: s.host_ns, end_ns: s.end_ns, key: s.key, data: self.reader.checked(&s.pack, &video::key(&s.sha), &s.sha)? };
            decoder.advance(&sample)?;
        }
        let s = &self.plan.epochs[e].samples[i];
        let sample = video::Sample { host_ns: s.host_ns, end_ns: s.end_ns, key: s.key, data: self.reader.checked(&s.pack, &video::key(&s.sha), &s.sha)? };
        let bgra = decoder.decode_bgra(&sample)?;
        self.at = Some((e, i));
        Ok(bgra)
    }
    #[cfg(not(target_os = "macos"))]
    fn decode(&mut self, _: usize, _: usize) -> Result<Vec<u8>> {
        bail!("HEVC screen recording needs macOS")
    }
}
/// Fits a BGRA frame into the output size, keeping its aspect ratio (black bars); nearest neighbour when the sizes
/// differ only in rare epochs of another resolution, an area average when shrinking.
fn scale(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, black: &[u8]) -> Vec<u8> {
    if sw == dw && sh == dh && src.len() == black.len() {
        return src.to_vec();
    }
    let mut out = black.to_vec();
    if sw == 0 || sh == 0 || src.len() < sw as usize * sh as usize * 4 {
        return out;
    }
    let k = (dw as f64 / sw as f64).min(dh as f64 / sh as f64);
    let (w, h) = (((sw as f64 * k) as u32).clamp(1, dw), ((sh as f64 * k) as u32).clamp(1, dh));
    let (x0, y0) = ((dw - w) / 2, (dh - h) / 2);
    let fx = sw as f64 / w as f64;
    let fy = sh as f64 / h as f64;
    for y in 0..h {
        let ya = (y as f64 * fy) as u32;
        let yb = (((y + 1) as f64 * fy) as u32).clamp(ya + 1, sh);
        for x in 0..w {
            let xa = (x as f64 * fx) as u32;
            let xb = (((x + 1) as f64 * fx) as u32).clamp(xa + 1, sw);
            let mut acc = [0u32; 3];
            let mut n = 0;
            // At most a 4x4 box per output pixel keeps big reductions fast.
            for yy in (ya..yb).step_by(((yb - ya) / 4).max(1) as usize) {
                for xx in (xa..xb).step_by(((xb - xa) / 4).max(1) as usize) {
                    let p = (yy as usize * sw as usize + xx as usize) * 4;
                    acc[0] += src[p] as u32;
                    acc[1] += src[p + 1] as u32;
                    acc[2] += src[p + 2] as u32;
                    n += 1;
                }
            }
            let o = ((y0 + y) as usize * dw as usize + (x0 + x) as usize) * 4;
            out[o] = (acc[0] / n) as u8;
            out[o + 1] = (acc[1] / n) as u8;
            out[o + 2] = (acc[2] / n) as u8;
        }
    }
    out
}

/// `.NAME.building` beside a clip: one build per output path, across processes. Holds the builder's pid; a claim
/// whose process is gone is stale and taken over.
struct Claim(PathBuf);
fn claim_path(out: &Path) -> PathBuf {
    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    out.with_file_name(format!(".{name}.building"))
}
impl Claim {
    fn take(out: &Path) -> Result<Self> {
        let path = claim_path(out);
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    write!(f, "{}", std::process::id())?;
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let pid = std::fs::read_to_string(&path).ok().and_then(|p| p.trim().parse::<i32>().ok());
                    let alive = pid.is_some_and(|p| p > 0 && (unsafe { libc::kill(p, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)));
                    ensure!(!alive, "CLIP_BUSY: {} is already being written", out.display());
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!("CLIP_BUSY: {} is already being written", out.display())
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
/// Whether `out`, its `.srt` or a build of it already exists.
pub fn taken(out: &Path) -> bool {
    out.exists() || out.with_extension("srt").exists() || claim_path(out).exists()
}

/// Builds the clip at `out` (`.mp4`, `.mov` or `.m4v`) and its `.srt` sidecar. The file appears only when complete.
#[cfg(target_os = "macos")]
pub fn build(root: &Path, options: &Options, out: &Path, more: &mut dyn FnMut() -> bool) -> Result<Report> {
    let _claim = Claim::take(out)?;
    let mut plan = Plan::new(root, options)?;
    let layout = plan.layout();
    let mut sink = writer::Writer::create(out, &layout)?;
    let mut report = match plan.write(&mut sink, more) {
        Ok(r) => r,
        Err(e) => {
            sink.cancel();
            return Err(e);
        }
    };
    let embedded = sink.finish()?;
    if layout.subtitles && !embedded {
        report.notes.push("this container refused a subtitle track; the subtitles are in the .srt beside it".into());
    }
    report.subtitles_embedded = embedded && layout.subtitles;
    let cues = plan.cues().to_vec();
    drop(plan);
    report.path = out.to_path_buf();
    report.bytes = std::fs::metadata(out)?.len();
    if !cues.is_empty() {
        let srt_path = out.with_extension("srt");
        // Beside the movie, in a folder the caller owns: no permission changes (unlike `atomic_write`).
        let partial = srt_path.with_extension(format!("srt.partial-{}", std::process::id()));
        std::fs::write(&partial, srt(&cues))?;
        std::fs::rename(&partial, &srt_path)?;
        report.srt = Some(srt_path);
    }
    Ok(report)
}

// ---------------------------------------------------------------- local time

/// Parses a local time for the CLI: "2026-10-24 00:40", "2026-10-24T00:40:30", "00:47" (on `base`'s day, or today),
/// RFC 3339 with an offset, "now", or relative "-10m" / "+90s" / "-2h" (from `base`, or now).
pub fn parse_time(text: &str, base: Option<u64>, now: u64) -> Result<u64> {
    let t = text.trim();
    if t == "now" {
        return Ok(now);
    }
    if let Some(sign @ ('+' | '-')) = t.chars().next() {
        let body = &t[1..];
        let (n, unit) = body.split_at(body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len()));
        let n: u64 = n.parse().context("relative time: a number, then s, m or h")?;
        let scale = match unit {
            "s" | "" => NS,
            "m" | "min" => 60 * NS,
            "h" => 3600 * NS,
            _ => bail!("relative time: use s, m or h"),
        };
        let from = base.unwrap_or(now);
        return Ok(if sign == '+' { from + n * scale } else { from.saturating_sub(n * scale) });
    }
    let (date, clock, zone) = split_time(t)?;
    let (y, mo, d) = match date {
        Some(d) => d,
        None => {
            let base = base.unwrap_or(now);
            let tm = local_tm((base / NS) as i64)?;
            (tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
        }
    };
    let (h, mi, s, nanos) = clock;
    let ns = match zone {
        Some(offset_s) => {
            let days = days_from_civil(y as i64, mo as u32, d as u32);
            let secs = days * 86_400 + h as i64 * 3600 + mi as i64 * 60 + s as i64 - offset_s;
            ensure!(secs >= 0, "time before 1970");
            secs as u64 * NS + nanos
        }
        None => {
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            tm.tm_year = y - 1900;
            tm.tm_mon = mo - 1;
            tm.tm_mday = d;
            tm.tm_hour = h;
            tm.tm_min = mi;
            tm.tm_sec = s;
            tm.tm_isdst = -1;
            let secs = unsafe { libc::mktime(&mut tm) };
            ensure!(secs >= 0, "unreadable local time {t}");
            secs as u64 * NS + nanos
        }
    };
    Ok(ns)
}
type Clock = (i32, i32, i32, u64);
fn split_time(t: &str) -> Result<(Option<(i32, i32, i32)>, Clock, Option<i64>)> {
    let bad = || anyhow::anyhow!("time \"{t}\": use \"2026-10-24 00:40\", \"00:47\", RFC 3339, now or -10m");
    let (date, rest) = match t.find([' ', 'T']) {
        Some(i) if t[..i].contains('-') => (Some(&t[..i]), &t[i + 1..]),
        _ if t.contains('-') && !t.contains(':') => (Some(t), "00:00"),
        _ => (None, t),
    };
    let date = match date {
        Some(d) => {
            let p: Vec<i32> = d.split('-').map(str::parse).collect::<Result<_, _>>().map_err(|_| bad())?;
            ensure!(p.len() == 3 && (1..=12).contains(&p[1]) && (1..=31).contains(&p[2]), bad());
            Some((p[0], p[1], p[2]))
        }
        None => None,
    };
    let (clock, zone) = if let Some(c) = rest.strip_suffix('Z') {
        (c, Some(0))
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let z = &rest[i + 1..];
        let (zh, zm) = z.split_once(':').unwrap_or((&z[..z.len().min(2)], &z[z.len().min(2)..]));
        let off = zh.parse::<i64>().map_err(|_| bad())? * 3600 + zm.parse::<i64>().unwrap_or(0) * 60;
        (&rest[..i], Some(if rest.as_bytes()[i] == b'-' { -off } else { off }))
    } else {
        (rest, None)
    };
    let parts: Vec<&str> = clock.split(':').collect();
    ensure!((2..=3).contains(&parts.len()), bad());
    let h: i32 = parts[0].parse().map_err(|_| bad())?;
    let mi: i32 = parts[1].parse().map_err(|_| bad())?;
    let (s, nanos) = match parts.get(2) {
        Some(sec) => {
            let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
            let digits: String = frac.chars().chain(std::iter::repeat('0')).take(9).collect();
            (whole.parse().map_err(|_| bad())?, if frac.is_empty() { 0 } else { digits.parse().map_err(|_| bad())? })
        }
        None => (0, 0),
    };
    ensure!((0..24).contains(&h) && (0..60).contains(&mi) && (0..61).contains(&s), bad());
    Ok((date, (h, mi, s, nanos), zone))
}
fn local_tm(secs: i64) -> Result<libc::tm> {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = secs;
    ensure!(!unsafe { libc::localtime_r(&t, &mut tm) }.is_null(), "local time");
    Ok(tm)
}
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
/// Where a clip goes when the caller names no file: `<data>/clips/heimdall-2026-10-24-0040-0047.mp4`, with a
/// counter when that name is taken.
pub fn default_out(data: &Path, from_ns: u64, to_ns: u64) -> Result<PathBuf> {
    let dir = data.join("clips");
    std::fs::create_dir_all(&dir)?;
    let (a, b) = (local_label(from_ns), local_label(to_ns));
    let stem = format!("heimdall-{}-{}{}", &a[..10], a[11..16].replace(':', ""), format!("-{}", b[11..16].replace(':', "")));
    for n in 1.. {
        let name = if n == 1 { format!("{stem}.mp4") } else { format!("{stem}-{n}.mp4") };
        let path = dir.join(name);
        if !taken(&path) {
            return Ok(path);
        }
    }
    unreachable!()
}
/// Seeing off covers archived screen reads too: the CLI honours the switch in Heimdall's settings
/// beside the archive (`<data>/settings.json`, `screen_off`), as `screen_frames` honours the running app's flag.
pub fn seeing_off(data: &Path) -> bool {
    std::fs::read(data.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["screen_off"] == true)
}
pub const SEEING_OFF: &str = "CAPTURE_UNAVAILABLE: no screen frames are available";

/// The CLI's window: `--to` given as a clock time is on the `--from` day, or the next day when that would be at
/// or before `--from` ("23:50" to "00:10"); a clock-only `--from` later than now means yesterday (at 00:15,
/// "23:50" is last night).
pub fn window(from: &str, to: &str, now: u64) -> Result<(u64, u64)> {
    let clock_only = |t: &str| t.contains(':') && t.trim().chars().all(|c| c.is_ascii_digit() || c == ':' || c == '.');
    let mut a = parse_time(from, None, now)?;
    if clock_only(from) && a > now {
        a -= DAY_NS;
    }
    let mut b = parse_time(to, Some(a), now)?;
    if clock_only(to) && b <= a {
        b += DAY_NS;
    }
    ensure!(b > a, "--to must be after --from");
    Ok((a, b))
}

/// "2026-10-24 00:40:00" in local time, for file names and answers.
pub fn local_label(ns: u64) -> String {
    match local_tm((ns / NS) as i64) {
        Ok(tm) => format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec),
        Err(_) => format!("{}", ns / NS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cues_split_long_speech_and_merge_overlaps() {
        let words: Vec<(String, u64, u64)> = (0..40).map(|i| (format!("w{i}"), 1000 * NS + i * 400_000_000, 1000 * NS + i * 400_000_000 + 300_000_000)).collect();
        let clip = |h: u64| h as i64 - 1000 * NS as i64;
        let cues = utterance_cues("User", "", &words, 1000 * NS, 1016 * NS, &clip);
        assert!(cues.len() >= 3, "{cues:?}");
        assert!(cues.iter().all(|c| c.end_ns - c.start_ns <= CUE_NS + CUE_HOLD_NS && c.text.starts_with("User: ")));
        assert!(cues.windows(2).all(|w| w[0].end_ns <= w[1].start_ns), "one speaker's lines never overlap: {cues:?}");
        let other = Cue { start_ns: 2 * NS, end_ns: 4 * NS, text: "Vim: hi".into() };
        let merged = merge_cues(cues.iter().cloned().chain([other]).collect(), 20 * NS);
        assert!(merged.windows(2).all(|w| w[0].end_ns <= w[1].start_ns));
        assert!(merged.iter().any(|c| c.text.contains("User: ") && c.text.contains("\nVim: hi")), "{merged:?}");
        let text = srt(&merged);
        assert!(text.starts_with("1\n00:00:00,000 --> "), "{text}");
        // Untimed text is spread over the utterance, never dropped.
        let untimed = utterance_cues("Heimdall", "one two three four five six seven", &[], 0, 12 * NS, &|h| h as i64);
        assert!(untimed.len() > 1);
        assert_eq!(untimed.iter().map(|c| c.text.matches(' ').count()).sum::<usize>(), 7, "every word, once: {untimed:?}");
    }
    #[test]
    fn labels_name_people_and_sources() {
        assert_eq!(speaker_label(Some("audio"), Some("ego lite")), "ego lite");
        assert_eq!(speaker_label(Some("User"), Some("Shure MV7i")), "User");
        let mic = r#"{"kind":{"InputDevice":{"name":"Shure MV7i","uid":"x"}}}"#;
        assert_eq!(describe_source("mic:x", mic), ("mic".into(), "Mic: Shure MV7i".into()));
        let call = r#"{"kind":{"Process":{"name":"Discord Helper","bundle_id":"com.hnc.Discord.helper","owner":{"name":"Discord","bundle_id":"com.hnc.Discord"}}}}"#;
        assert_eq!(describe_source("process:1", call).1, "Call: Discord Helper");
        let me = r#"{"kind":{"Process":{"name":"Heimdall","bundle_id":"dev.heimdall.app","owner":{"name":"Heimdall","bundle_id":"dev.heimdall.app"}}}}"#;
        assert_eq!(describe_source("process:2", me).0, "heimdall");
    }
    #[test]
    fn times_parse_in_local_and_explicit_zones() {
        let now = 1_792_800_000 * NS;
        let from = parse_time("2026-10-24 00:40", None, now).unwrap();
        assert_eq!(local_label(from), "2026-10-24 00:40:00");
        let to = parse_time("00:47", Some(from), now).unwrap();
        assert_eq!(to - from, 7 * 60 * NS);
        assert_eq!(parse_time("+90s", Some(from), now).unwrap() - from, 90 * NS);
        assert_eq!(parse_time("-10m", None, now).unwrap(), now - 600 * NS);
        assert_eq!(parse_time("2026-10-23T22:40:00Z", None, now).unwrap(), parse_time("2026-10-24T00:40:00+02:00", None, now).unwrap());
        assert_eq!(parse_time("1970-01-01T00:00:01.5Z", None, now).unwrap(), 1_500_000_000);
        assert!(parse_time("yesterday", None, now).is_err());
    }
    #[test]
    fn a_clock_window_crosses_midnight_both_ways() {
        let now = parse_time("2026-10-24 00:15", None, 0).unwrap();
        let (a, b) = window("23:50", "00:10", now).unwrap();
        assert_eq!((local_label(a), local_label(b)), ("2026-10-23 23:50:00".into(), "2026-10-24 00:10:00".into()));
        let (a, b) = window("2026-10-24 00:40", "00:47", now).unwrap();
        assert_eq!(b - a, 7 * 60 * NS);
        assert_eq!(local_label(a), "2026-10-24 00:40:00", "an explicit date is kept even in the future");
        assert!(window("00:10", "-5m", now).is_err());
    }
    /// A crashed launch ends where the next starts; a sleep inside a launch (host clock paused, wall − host jumps)
    /// splits it, so the time after waking is laid out by wall clock instead of shifted early.
    #[test]
    fn launches_and_sleeps_split_the_window_by_wall_clock() {
        let t = tempfile::tempdir().unwrap();
        let db = store::open(t.path()).unwrap();
        let w = 1_800_000_000 * NS;
        let a = store::ensure_session(&db, "session-a").unwrap();
        db.execute("UPDATE sessions SET started_wall_ns=?1 WHERE id=?2", params![w as i64, a]).unwrap();
        store::create_segment(&db, a, "capture-a", w, 0, "native").unwrap();
        // 30 s asleep after host 50 s: the next video epoch starts at host 50 s, wall +80 s (anchor 0.3 s coarse).
        db.execute("INSERT INTO video_segments(session,display,geometry,format,start_ns,end_ns,wall_ns,fps,tier) VALUES(?1,1,'{}','{}',?2,?3,?4,5,'hot')", params![a, 50 * NS as i64, 60 * NS as i64, (w + 80 * NS + 300_000_000) as i64]).unwrap();
        db.execute("INSERT INTO video_segments(session,display,geometry,format,start_ns,end_ns,wall_ns,fps,tier) VALUES(?1,1,'{}','{}',?2,?3,?4,5,'hot')", params![a, 20 * NS as i64, 30 * NS as i64, (w + 20 * NS + 200_000_000) as i64]).unwrap();
        let b = store::ensure_session(&db, "session-b").unwrap();
        db.execute("UPDATE sessions SET started_wall_ns=?1 WHERE id=?2", params![(w + 100 * NS) as i64, b]).unwrap();
        store::create_segment(&db, b, "capture-b", w + 100 * NS, 1000 * NS, "native").unwrap();
        let got: Vec<(i64, u64, u64, u64)> = pieces(&db, w + 10 * NS, w + 120 * NS).unwrap().iter().map(|p| (p.session, p.host_from / NS, p.host_to / NS, p.offset_ns / NS)).collect();
        // Launch a: host 10-50 s at clip 0; after waking host 50-70.0 s (wall +80.3-100) at clip 70.3 s;
        // launch b: host 1000-1020 s at clip 90 s.
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], (a, 10, 50, 0));
        assert_eq!((got[1].0, got[1].1, got[1].3), (a, 50, 70));
        assert_eq!(got[2], (b, 1000, 1020, 90));
    }
    /// 44.1 kHz callbacks on a 48 kHz track (and 16 kHz cold audio on a 44.1 kHz one) abut exactly: no frame is
    /// lost per callback, so the cursor never drifts into a re-anchor click.
    #[test]
    fn callback_spans_abut_at_any_pair_of_rates() {
        for (track, block) in [(48_000, 44_100), (44_100, 44_100), (44_100, 16_000)] {
            let placer = Placer::new(track, 1);
            let spans: Vec<(u64, u64)> = (0..1000u64)
                .map(|k| {
                    let b = Block { piece: 0, host_ns: 0, rate: block, channels: 1, frames: 512, codec: Arc::from("f32-zstd"), object: None, offset: k * 512, gain: 1. };
                    placer.span(&b)
                })
                .collect();
            assert!(spans.windows(2).all(|w| w[0].1 == w[1].0), "{track}/{block}");
            let total = spans.last().unwrap().1 - spans[0].0;
            assert_eq!(total, (512_000 * track as u64 + block as u64 / 2) / block as u64);
        }
    }
    #[test]
    fn scaling_letterboxes_and_keeps_colour() {
        let black = [0, 0, 0, 255].repeat(8 * 8);
        let red: Vec<u8> = [0, 0, 255, 255].repeat(16 * 8);
        let out = scale(&red, 16, 8, 8, 8, &black);
        assert_eq!(&out[..4], &[0, 0, 0, 255], "bars above a wide frame");
        let mid = (4 * 8 + 4) * 4;
        assert_eq!(&out[mid..mid + 4], &[0, 0, 255, 255]);
    }
}
