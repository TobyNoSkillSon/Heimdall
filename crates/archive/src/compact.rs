use crate::{
    DAY_NS, audio, chunks, hash,
    records::{self, DICTIONARY},
    store, wall_ns,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Arc, Condvar, Mutex, Weak},
    time::Duration,
};

pub const COLD_BITRATE: i32 = 32_000; // Opus experiment rate; shipping codec is COLD_CODEC.
pub const COLD_CODEC: &str = "flac16";
#[derive(Default, Debug)]
pub struct Receipt {
    pub audio_groups: usize,
    pub images: usize,
    pub video_segments: usize,
    pub payloads: usize,
    pub chunks: usize,
    pub reclaimed_objects: usize,
    pub migrated_sessions: usize,
    pub errors: Vec<String>,
}
impl Receipt {
    fn add(&mut self, o: Receipt) {
        self.audio_groups += o.audio_groups;
        self.images += o.images;
        self.video_segments += o.video_segments;
        self.payloads += o.payloads;
        self.chunks += o.chunks;
        self.reclaimed_objects += o.reclaimed_objects;
        self.migrated_sessions += o.migrated_sessions;
        self.errors.extend(o.errors);
    }
    fn worked(&self) -> bool {
        self.video_segments > 0 || self.audio_groups > 0 || self.payloads > 0 || self.images > 0 || self.chunks > 0 || self.migrated_sessions > 0
    }
}
fn lock_file(dir: &Path, name: &str) -> Result<File> {
    if !dir.is_dir() { crate::private_dir(dir)?; }
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(name))?)
}
fn flock(file: &File, op: libc::c_int) -> bool {
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), op) == 0 }
}
/// The exclusive lifetime lock: held only while blobs are deleted (`store::collect`, `store::sweep`) or a video epoch's
/// samples are replaced, one bounded writer job at a time, taken without waiting. Readers hold it shared.
pub(crate) struct Lock {
    file: File,
}
pub(crate) fn exclusive_lock(dir: &Path) -> Result<Option<Lock>> {
    let file = lock_file(dir, ".compact.lock")?;
    Ok(flock(&file, libc::LOCK_EX | libc::LOCK_NB).then_some(Lock { file }))
}
impl Drop for Lock {
    fn drop(&mut self) {
        flock(&self.file, libc::LOCK_UN);
    }
}
/// One compactor per store across processes (the app, `heimdall-archive compact`): held for a whole pass, never by a
/// reader, so a reader never waits for compaction.
pub(crate) fn owner(dir: &Path) -> Result<Option<File>> {
    // The lifetime lock file exists once anything was compacted: read-only tools take it shared and never create it.
    lock_file(dir, ".compact.lock")?;
    let file = lock_file(dir, ".compact.owner")?;
    Ok(flock(&file, libc::LOCK_EX | libc::LOCK_NB).then_some(file))
}
/// Removes queued blobs, `store::COLLECT_KEYS` per writer job, while `more()` allows. Returns blobs removed.
pub(crate) fn collect_all(queue: &crate::live::Queue, root: &Path, more: &mut dyn FnMut() -> bool) -> Result<usize> {
    let mut removed = 0;
    loop {
        let r = root.to_path_buf();
        let (n, queued) = queue.background(move |db| {
            let n = store::collect(db, &r, store::COLLECT_KEYS)?;
            let left: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM gc_blobs)", [], |r| r.get(0))?;
            Ok((n, left))
        })?;
        removed += n;
        if n == 0 || !queued || !more() {
            return Ok(removed);
        }
    }
}
/// Frees up to `pages` pages of the index, `store::RECLAIM_PAGES` per writer job.
pub(crate) fn reclaim_index(queue: &crate::live::Queue, pages: u32, more: &mut dyn FnMut() -> bool) -> Result<()> {
    let mut left = pages;
    while left > 0 && more() {
        let step = left.min(store::RECLAIM_PAGES);
        let free: i64 = queue.background(move |db| {
            store::reclaim(db, step)?;
            Ok(db.query_row("PRAGMA freelist_count", [], |r| r.get(0))?)
        })?;
        if free == 0 {
            break;
        }
        left -= step;
    }
    Ok(())
}
/// Drains the blob collection queue now and returns freed pages (after a migration run, the CLI).
/// Returns `None` when another compactor owns the store.
pub fn collect_now(root: &Path) -> Result<Option<usize>> {
    let Some(_owner) = owner(root)? else { return Ok(None) };
    let queue = crate::live::Queue::shared(root)?;
    let n = collect_all(&queue, root, &mut || true)?;
    for pack in store::packs(root)? {
        store::reclaim_pack(root, &pack, u32::MAX)?;
    }
    reclaim_index(&queue, u32::MAX, &mut || true)?;
    Ok(Some(n))
}
pub struct ReaderLock(File, u64);
/// Shared archive lock: nothing a reader sees is deleted while it holds it. Waits at most one collection job.
pub fn reader_lock(root: &Path) -> Result<ReaderLock> {
    let file = lock_file(root, ".compact.lock")?;
    ensure!(flock(&file, libc::LOCK_SH), "archive read lock");
    let token = NEXT_READER.fetch_add(1, Ordering::Relaxed);
    let thread = std::thread::current().name().unwrap_or("an unnamed thread").to_owned();
    readers().lock().unwrap().push((token, thread, std::time::Instant::now()));
    Ok(ReaderLock(file, token))
}
impl Drop for ReaderLock {
    fn drop(&mut self) {
        readers().lock().unwrap().retain(|(t, ..)| *t != self.1);
        flock(&self.0, libc::LOCK_UN);
    }
}
static NEXT_READER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The reader locks this process holds: the thread that took each one and since when (`readers_held`).
fn readers() -> &'static Mutex<Vec<(u64, String, std::time::Instant)>> {
    static READERS: Mutex<Vec<(u64, String, std::time::Instant)>> = Mutex::new(Vec::new());
    &READERS
}
/// Who holds the archive's reader lock in this process, longest first ("heimdall-embeddings for 4.2 s"), or "another
/// process" when no thread here does: the stall lines name the reader instead of leaving it to be guessed (13 Nov:
/// tiering stalled all day behind a reader nobody could name).
pub(crate) fn readers_held() -> String {
    let mut held: Vec<(String, std::time::Duration)> =
        readers().lock().unwrap().iter().map(|(_, thread, since)| (thread.clone(), since.elapsed())).collect();
    if held.is_empty() {
        return "another process".into();
    }
    held.sort_by(|a, b| b.1.cmp(&a.1));
    held.iter().map(|(thread, age)| format!("{thread} for {:.1} s", age.as_secs_f64())).collect::<Vec<_>>().join(", ")
}
/// Audio groups one pass may publish before metadata packing and the caller's gate get a turn.
pub const PASS_AUDIO_GROUPS: usize = 32;
/// Metadata chunks one pass may pack per kind (timeline, evidence).
pub const PASS_CHUNKS: usize = 16;
/// Allows `n` calls.
fn budget(mut n: usize) -> impl FnMut() -> bool {
    move || {
        let more = n > 0;
        n = n.saturating_sub(1);
        more
    }
}
/// Audio a group spans at most: one cold object, one writer job (about 1,000 capture rows at 100 a second).
pub const GROUP_NS: u64 = 10_000_000_000;
/// One bounded group per writer job. Readers see old or verified new.
/// Before committing refs, the new object is written, fsynced and independently decoded.
pub fn compact_audio(root: &Path, now: u64, _bitrate: i32, max_groups: usize) -> Result<Receipt> {
    compact_audio_while(root, now, &mut budget(max_groups))
}
/// `compact_audio` asking `more()` before each group, oldest eligible audio first. A started
/// group finishes (publication and reference move are one transaction); none starts after
/// `more()` returns false. A segment whose group fails is skipped for the rest of the call,
/// so it cannot hold up other segments.
pub fn compact_audio_while(root: &Path, now: u64, more: &mut dyn FnMut() -> bool) -> Result<Receipt> {
    let Some(_owner) = owner(root)? else {
        return Ok(Receipt::default());
    };
    let queue = crate::live::Queue::shared(root)?;
    let db = store::open(root)?;
    let cutoff = now.saturating_sub(DAY_NS);
    let mut result = Receipt::default();
    result.reclaimed_objects += collect_all(&queue, root, more)?;
    let mut skip: Vec<i64> = vec![];
    let mut cold_until = BTreeMap::<(i64, String), u64>::new();
    let mut objects = audio::ObjectCache::default();
    while more() {
        let excluded = skip.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let first: Option<(i64, i64, String, u64, u32, u16)> = db
            .prepare_cached(&format!(
                "SELECT b.id,b.segment,b.source_id,b.host_ns,b.rate,b.channels FROM blocks b INDEXED BY block_age WHERE b.codec='f32-zstd' AND b.wall_ns<=?1 AND b.wall_ns+b.frames*1000000000/b.rate<=?1 AND b.segment NOT IN ({excluded}) ORDER BY b.wall_ns,b.id LIMIT 1"
            ))?
            .query_row([cutoff as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? as u64, r.get(4)?, r.get(5)?)))
            .optional()?;
        let Some((_, segment, source, start, rate, channels)) = first else { break };
        let group = prepare_group(&db, root, segment, &source, start, rate, channels, cutoff, &mut objects);
        let published = group.and_then(|g| {
            let (session, end, r) = (g.session, g.end, root.to_path_buf());
            let reclaimed = queue.background(move |w| publish_group(w, &r, &g))?;
            Ok((session, end, reclaimed))
        });
        match published {
            Ok((session, end, reclaimed)) => {
                crash_point("committed");
                result.audio_groups += 1;
                result.reclaimed_objects += reclaimed;
                let until = cold_until.entry((session, source)).or_default();
                *until = (*until).max(end);
            }
            Err(e) => {
                result.errors.push(format!("segment {segment} {source}@{start}: {e:#}"));
                skip.push(segment);
            }
        }
    }
    for ((session, source), end) in cold_until {
        // VAD runs of audio now cold, `ACTIVITY_ROWS` per writer job.
        loop {
            let s = source.clone();
            let n = queue.background(move |w| records::discard_activity_in(w, session, &s, end, ACTIVITY_ROWS))?;
            if n < ACTIVITY_ROWS {
                break;
            }
        }
    }
    result.reclaimed_objects += collect_all(&queue, root, more)?;
    if result.audio_groups > 0 {
        store::reclaim_pack(root, store::HOT, 8192 * (result.audio_groups as u32 + 1))?;
        reclaim_index(&queue, 2048 * (result.audio_groups as u32 + 1), more)?;
    }
    Ok(result)
}
/// VAD activity rows one writer job discards at most.
pub const ACTIVITY_ROWS: usize = 512;
/// One audio group, encoded, stored in its month pack and verified: everything but the reference move.
struct Group {
    session: i64,
    source: String,
    start: u64,
    end: u64,
    rows: Vec<(i64, usize, Option<String>)>,
    equivalent: Option<(String, i64, f32)>,
    quality: i64,
    fingerprint: String,
    id: String,
    pack: String,
    key: String,
    bytes: Vec<u8>,
    rate: u32,
    channels: u16,
    frames: i64,
    codec: &'static str,
    stored_sha: String,
}
#[allow(clippy::too_many_arguments)]
fn prepare_group(
    db: &Connection,
    root: &Path,
    segment: i64,
    source: &str,
    start: u64,
    rate: u32,
    channels: u16,
    cutoff: u64,
    objects: &mut audio::ObjectCache,
) -> Result<Group> {
    let session: i64 = db
        .prepare_cached("SELECT session FROM segments WHERE id=?1")?
        .query_row([segment], |r| r.get(0))?;
    let candidates: Vec<(i64, u64, usize, bool, u64, Option<String>)> = db
        .prepare_cached("SELECT id,host_ns,frames,discontinuity,wall_ns,object_id FROM blocks WHERE segment=?1 AND source_id=?2 AND host_ns>=?3 AND host_ns<?4 AND wall_ns+frames*1000000000/rate<=?5 AND codec='f32-zstd' AND rate=?6 AND channels=?7 ORDER BY host_ns,id LIMIT 8192")?
        .query_map(
            params![segment, source, start as i64, (start + GROUP_NS) as i64, cutoff as i64, rate, channels],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as usize, r.get(3)?, r.get::<_, i64>(4)? as u64, r.get(5)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let ranges = records::non_speech_ranges_in(db, session, source, start, start + GROUP_NS)?;
    let is_low = |a: u64, b: u64| ranges.iter().any(|(x, y)| *x <= a && *y >= b);
    let low = candidates
        .first()
        .is_some_and(|(_, ns, n, _, _, _)| is_low(*ns, *ns + *n as u64 * 1_000_000_000 / rate as u64));
    let wall = candidates.first().map_or(cutoff, |c| c.4);
    let mut pcm = vec![];
    let mut rows = vec![];
    let mut expected = start;
    let mut frames = 0usize;
    for (id, ns, n, discontinuous, _, object) in candidates {
        if !rows.is_empty()
            && (discontinuous
                || ns.abs_diff(expected) > 1_000_000_000 / rate as u64
                || (frames + n) as u64 * 1_000_000_000 > rate as u64 * GROUP_NS
                || is_low(ns, ns + n as u64 * 1_000_000_000 / rate as u64) != low)
        {
            break;
        }
        let p = audio::read_block_cached(db, id, objects)?;
        ensure!(p.rate == rate && p.channels == channels, "hot geometry mismatch");
        rows.push((id, frames, object));
        pcm.extend(p.samples);
        frames += n;
        expected = ns + n as u64 * 1_000_000_000 / rate as u64;
    }
    ensure!(!rows.is_empty(), "compactor made no progress");
    let mono = audio::mono_16k(&pcm, rate, channels)?;
    // Exceptional PCM stays in its original geometry and must not stall other sources.
    let native_fallback = mono.is_empty() || !mono.iter().all(|x| x.is_finite());
    let fingerprint = if native_fallback {
        format!("native:{}", hash(&audio::pcm_bytes(&pcm)))
    } else {
        signal_fingerprint(&mono)
    };
    let quality = source_quality(db, segment, source, rate)?;
    let voice = voice_source(db, segment, source)?;
    let target_rate = if !voice { 16000 } else if low { audio::opus_rate(rate) } else { rate };
    let matches: Vec<(String, i64)> = db
        .prepare_cached("SELECT object_id,quality FROM audio_spans WHERE fingerprint=?1 AND abs(start_ns-?2)<=62500 AND abs(end_ns-?3)<=62500 LIMIT 8")?
        .query_map(params![fingerprint, start as i64, expected as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut equivalent = None;
    for (object, prior_quality) in matches {
        if native_fallback {
            break;
        }
        let prior_codec: Option<String> = db
            .prepare_cached("SELECT codec FROM audio_objects WHERE id=?1")?
            .query_row([&object], |r| r.get(0))
            .optional()?;
        let Some(prior_codec) = prior_codec else { continue };
        // Speech/unknown coverage cannot inherit a lower non-speech codec
        // merely because another source's VAD classified the same span low.
        if !low && (prior_codec == "opus-har1" || prior_codec == audio::OPUS_NATIVE) {
            continue;
        }
        let old = audio::read_object(db, &object)?;
        // Nor a voice source's capture-rate audio a lower rate or fewer channels stored for another source.
        if voice && (old.rate < target_rate || (!low && old.channels < cold_channels(&pcm, channels))) {
            continue;
        }
        // The group's blocks must fit inside the shared object at its rate (the read path maps frames by rate ratio).
        let old_frames = old.samples.len() / old.channels as usize;
        if (frames as u64 * old.rate as u64).div_ceil(rate as u64) > old_frames as u64 {
            continue;
        }
        // Resampled objects are padded to the rounded-up length (`fit`); compare the common length.
        let mut old_mono = audio::mono_16k(&old.samples, old.rate, old.channels)?;
        if (mono.len()..=mono.len() + 2).contains(&old_mono.len()) {
            old_mono.truncate(mono.len());
        }
        if let Some(gain) = same_signal(&mono, &old_mono) {
            let packed: bool = db
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM packed_audio_refs WHERE object_id=?1)")?
                .query_row([&object], |r| r.get(0))?;
            // A signal already stored at a higher rate (a voice source's) is kept, never replaced by a 16 kHz copy.
            // Nor may this group replace another source's richer object (more channels, FLAC under its Opus).
            let richer = old.rate > target_rate
                || old.channels > if voice && !low { cold_channels(&pcm, channels) } else { 1 }
                || (low && !prior_codec.starts_with("opus"));
            equivalent = Some((object, if packed || richer { i64::MAX } else { prior_quality }, gain));
            break;
        }
    }
    let cold = if native_fallback {
        Cold { codec: "f32-zstd", rate, channels, bytes: zstd::bulk::compress(&audio::pcm_bytes(&pcm), 19)?, samples: pcm.len() }
    } else {
        encode_cold(&pcm, rate, channels, &mono, voice, low)?
    };
    let Cold { codec, rate: encoded_rate, channels: encoded_channels, bytes, samples: encoded_len } = cold;
    let stored_sha = hash(&bytes);
    let mut identity = encoded_rate.to_le_bytes().to_vec();
    identity.extend(encoded_channels.to_le_bytes());
    identity.extend(&bytes);
    let id = hash(&identity);
    let pack = store::month(wall);
    let key = store::audio_key(&id);
    store::put(root, &pack, &[(key.clone(), &bytes)])?;
    let stored = store::get(root, &pack, &key)?.context("cold object vanished after commit")?;
    let frames = encoded_len / encoded_channels as usize;
    let checked = audio::decode_object(encoded_rate, encoded_channels, frames, codec, &stored)?;
    // Read back byte for byte and decoded independently. Float objects decode deterministically from those bytes; they
    // hold the policy's samples (16 kHz mono for an app), not always `pcm` (13 Nov: comparing with `pcm` failed every
    // app group with a sample outside ±1 on a real day).
    ensure!(
        stored == bytes
            && checked.samples.len() == encoded_len
            && (codec == "f32-zstd" || checked.samples.iter().all(|x| x.is_finite())),
        "cold verification failed"
    );
    crash_point("verified");
    let frames = frames as i64;
    Ok(Group {
        session,
        source: source.to_owned(),
        start,
        end: expected,
        rows,
        equivalent,
        quality,
        fingerprint,
        id,
        pack,
        key,
        bytes,
        rate: encoded_rate,
        channels: encoded_channels,
        frames,
        codec,
        stored_sha,
    })
}
/// The reference move, in one writer job: the group's rows point at the cold object (or at an equivalent one already
/// there), and objects nothing references any more lose their catalogue row (their blobs are collected later). Fails
/// (and changes nothing) when a row is no longer the hot row the group read.
fn publish_group(db: &Connection, root: &Path, g: &Group) -> Result<usize> {
    let mut id = g.id.clone();
    let mut gain = 1f32;
    let mut obsolete: Vec<String> = g.rows.iter().filter_map(|(_, _, o)| o.clone()).collect();
    let catalogue = |id: &str| -> Result<()> {
        store::ensure_blob(root, &g.pack, &g.key, &g.bytes)?;
        db.prepare_cached("INSERT OR IGNORE INTO audio_objects VALUES(?1,?2,?3,?4,?5,?6,?7)")?
            .execute(params![id, g.rate, g.channels, g.frames, g.codec, g.pack, g.stored_sha])?;
        Ok(())
    };
    if let Some((old, old_quality, ratio)) = &g.equivalent {
        if *old_quality >= g.quality {
            id = old.clone();
            gain = *ratio;
        } else {
            catalogue(&id)?;
            db.prepare_cached("UPDATE blocks SET object_id=?2,gain=gain/?3 WHERE object_id=?1")?
                .execute(params![old, id, ratio])?;
            obsolete.push(old.clone());
            db.prepare_cached("UPDATE audio_spans SET object_id=?2,quality=?3 WHERE object_id=?1")?
                .execute(params![old, id, g.quality])?;
        }
    } else {
        catalogue(&id)?;
    }
    for (row, offset, _) in &g.rows {
        let moved = db
            .prepare_cached("UPDATE blocks SET codec='cold-ref',object_id=?2,offset_frames=?3,gain=?4 WHERE id=?1 AND codec='f32-zstd'")?
            .execute(params![row, id, *offset as i64, gain])?;
        ensure!(moved == 1, "hot row {row} changed while its group was encoded");
    }
    db.prepare_cached("INSERT INTO audio_spans VALUES(?1,?2,?3,?4,?5,?6)")?
        .execute(params![g.source, g.start as i64, g.end as i64, id, g.quality, g.fingerprint])?;
    if id != g.id {
        // An equivalent existing signal was kept; the new blob is not referenced.
        store::queue_gc(db, &g.pack, &g.key)?;
    }
    obsolete.sort();
    obsolete.dedup();
    let mut reclaimed = 0;
    for old in &obsolete {
        if *old == id {
            continue;
        }
        let pack_of: Option<String> = db
            .prepare_cached("SELECT pack FROM audio_objects WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM blocks WHERE object_id=?1) AND NOT EXISTS(SELECT 1 FROM packed_audio_refs WHERE object_id=?1) AND NOT EXISTS(SELECT 1 FROM clips WHERE object_id=?1)")?
            .query_row([old], |r| r.get(0))
            .optional()?;
        if let Some(p) = pack_of {
            db.prepare_cached("DELETE FROM audio_objects WHERE id=?1")?.execute([old])?;
            store::queue_gc(db, &p, &store::audio_key(old))?;
            reclaimed += 1;
        }
    }
    Ok(reclaimed)
}
/// Non-speech of a voice source: mono Opus at the capture rate (48 kHz when Opus does not take it), fullband.
pub const VOICE_NON_SPEECH_BITRATE: i32 = 32_000;
/// One cold encoding of a group: codec, catalogued geometry, bytes, decoded sample count.
pub struct Cold {
    pub codec: &'static str,
    pub rate: u32,
    pub channels: u16,
    pub bytes: Vec<u8>,
    pub samples: usize,
}
/// Channels a voice source's speech keeps: one when every frame's channels are equal (a mono microphone presented as
/// stereo, the Shure MV7i), else up to two; more than two are mixed to one.
pub fn cold_channels(pcm: &[f32], channels: u16) -> u16 {
    let c = channels as usize;
    if c == 2 && !pcm.chunks_exact(2).all(|f| f[0] == f[1]) { 2 } else { 1 }
}
/// The cold codec policy (13 Nov; "can you store a proper high quality audio? I never said to store low
/// quality audio."). `pcm` is the group's native interleaved audio, `mono` its mono 16 kHz copy, `low` whether VAD
/// covers it as non-speech.
/// - Voice sources (microphones, the phone's mic, calls, Heimdall's voice): speech and unknown coverage as FLAC 16-bit at
///   the capture rate, mono unless the channels differ (≤2); non-speech as mono Opus at the capture rate, 32 kb/s.
/// - Other sources (apps, media): unchanged since 11 Oct, FLAC 16 kHz mono for speech, Opus 12 kb/s 16 kHz for the rest.
/// - Values outside [-1, 1] are never clipped: they stay float (zstd) in the geometry the policy chose.
pub fn encode_cold(pcm: &[f32], rate: u32, channels: u16, mono: &[f32], voice: bool, low: bool) -> Result<Cold> {
    let original_rate = rate;
    // The native frame count must use the native channel count: the binding below shadows `channels` with the cold
    // copy's (review B1, 13 Nov: stereo app audio went cold at twice its length).
    let native_frames = (pcm.len() / channels.max(1) as usize) as u64;
    let (samples, rate, channels): (std::borrow::Cow<[f32]>, u32, u16) = if !voice {
        (mono.into(), 16000, 1)
    } else if low {
        (audio::mono_at(pcm, rate, channels, audio::opus_rate(rate))?.into(), audio::opus_rate(rate), 1)
    } else {
        match cold_channels(pcm, channels) {
            c if c == channels => (pcm.into(), rate, c),
            1 if channels == 2 => (pcm.iter().step_by(2).copied().collect::<Vec<_>>().into(), rate, 1),
            _ => (audio::mono_at(pcm, rate, channels, rate)?.into(), rate, 1),
        }
    };
    // A resampled object holds the rounded-up length, so the last callback of a group (whose end the clip reader rounds
    // up) always fits: 16 kHz objects of 48 kHz callbacks lost up to 0.3 s per 5 minutes in clips to "callback exceeds
    // its audio object" (10 Nov), and 44.1 kHz voice non-speech stored at 48 kHz would have too (review, 13 Nov).
    let samples = fit(samples, native_frames, original_rate, rate);
    let n = samples.len();
    if !samples.iter().all(|x| (-1.0..=1.0).contains(x)) {
        return Ok(Cold { codec: "f32-zstd", rate, channels, bytes: zstd::bulk::compress(&audio::pcm_bytes(&samples), 19)?, samples: n });
    }
    Ok(match (voice, low) {
        (false, true) => Cold { codec: "opus-har1", rate, channels, bytes: audio::encode_opus(&samples, 12_000)?, samples: n },
        (true, true) => Cold { codec: audio::OPUS_NATIVE, rate, channels, bytes: audio::encode_opus_native(&samples, rate, VOICE_NON_SPEECH_BITRATE)?, samples: n },
        (_, false) => Cold { codec: COLD_CODEC, rate, channels, bytes: audio::encode_flac_as(&samples, rate, channels)?, samples: n },
    })
}
/// Pads a mono resampled copy of `frames` native frames to ceil(frames × out / native) with its last value.
fn fit(samples: std::borrow::Cow<'_, [f32]>, frames: u64, native: u32, out: u32) -> std::borrow::Cow<'_, [f32]> {
    let want = (frames * out as u64).div_ceil(native as u64) as usize;
    if native == out || samples.len() >= want || samples.is_empty() {
        return samples;
    }
    let mut v = samples.into_owned();
    let last = *v.last().unwrap();
    v.resize(want, last);
    v.into()
}
/// Microphones (the Mac's and the phone's), calls and Heimdall's own voice; a source without a catalogue entry counts
/// by its ID (`mic:…`).
fn voice_source(db: &Connection, segment: i64, source: &str) -> Result<bool> {
    let metadata: Option<String> = db
        .prepare_cached("SELECT metadata_json FROM sources WHERE segment=?1 AND id=?2")?
        .query_row(params![segment, source], |r| r.get(0))
        .optional()?;
    Ok(match metadata {
        Some(m) => match crate::clip::describe_source(source, &m).0.as_str() {
            "mic" | "call" | "heimdall" => true,
            "audio" => source.starts_with("mic"),
            _ => false,
        },
        None => source.starts_with("mic"),
    })
}
fn source_quality(db: &Connection, segment: i64, source: &str, rate: u32) -> Result<i64> {
    let metadata: Option<String> = db
        .prepare_cached("SELECT metadata_json FROM sources WHERE segment=?1 AND id=?2")?
        .query_row(params![segment, source], |r| r.get(0))
        .optional()?;
    let digital = metadata
        .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
        .is_some_and(|v| v["kind"].get("Process").is_some());
    Ok(if digital { 1_000_000 } else { 0 } + rate as i64)
}
fn signal_fingerprint(pcm: &[f32]) -> String {
    let peak = pcm.iter().map(|x| x.abs()).fold(0f32, f32::max);
    if peak == 0. {
        return "zero".into();
    }
    let mut bytes = (pcm.len() as u64).to_le_bytes().to_vec();
    for x in pcm.iter().step_by(16) {
        bytes.push((x / peak * 63.).round() as i8 as u8);
    }
    hash(&bytes)
}
/// Conservative linear-equivalence gate. Distinct sources/utterances stay distinct.
/// Gain stays in each reference; canonical source preference is direct digital then native rate.
fn same_signal(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.len() < 8000 {
        return None;
    }
    let aa = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
    let bb = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>();
    let ab = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum::<f64>();
    if aa < 1e-10 || bb < 1e-10 || ab / (aa * bb).sqrt() < 0.99999 {
        return None;
    }
    let gain = (ab / bb) as f32;
    if !(0.05..=20.).contains(&gain) {
        return None;
    }
    let residual = a.iter().zip(b).map(|(x, y)| (x - y * gain).abs()).fold(0f32, f32::max);
    (residual <= 1.0 / 16384.0 * gain.abs().max(1.)).then_some(gain)
}
// Test-only failpoints are enabled explicitly by the lab CLI.
static CRASH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
fn crash_id(stage: &str) -> u8 {
    match stage {
        "verified" => 1,
        "committed" => 2,
        "chunk_verified" => 5,
        "chunk_committed" => 6,
        "migrate_copied" => 7,
        "migrate_verified" => 8,
        _ => 0,
    }
}
pub fn set_crash_test(stage: &str) {
    CRASH.store(crash_id(stage), std::sync::atomic::Ordering::Relaxed);
}
pub(crate) fn crash_point(stage: &str) {
    if CRASH.load(std::sync::atomic::Ordering::Relaxed) == crash_id(stage) {
        unsafe {
            libc::kill(std::process::id() as i32, libc::SIGKILL);
        }
    }
}

/// Payload bodies one writer job recompresses at most.
pub const PAYLOAD_STEP: usize = 64;
/// Cold payload bodies (zstd19 + dictionary) and cold screenshots (pixel-exact WebP when smaller,
/// else the original) move to their month pack; the hot copies are collected. Bodies are recompressed here,
/// `PAYLOAD_STEP` per writer job; each screenshot is converted here and moved by its own writer job.
pub fn compact_records(root: &Path, now: u64) -> Result<Receipt> {
    compact_records_while(root, now, &mut || true)
}
fn compact_records_while(root: &Path, now: u64, more: &mut dyn FnMut() -> bool) -> Result<Receipt> {
    let Some(_owner) = owner(root)? else {
        return Ok(Receipt::default());
    };
    let queue = crate::live::Queue::shared(root)?;
    let db = store::open(root)?;
    let cutoff = now.saturating_sub(DAY_NS);
    let mut result = Receipt::default();
    let payloads: Vec<(String, usize, Vec<u8>)> = db
        .prepare("SELECT DISTINCT p.id,p.raw_bytes,p.data FROM evidence e INDEXED BY evidence_time CROSS JOIN payloads p ON p.id=e.payload_id WHERE e.wall_ns<=?1 AND p.codec='json-zstd-dict-v1' AND NOT EXISTS(SELECT 1 FROM evidence h WHERE h.payload_id=p.id AND h.wall_ns>?1) LIMIT 1024")?
        .query_map([cutoff as i64], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
    let mut enc = zstd::bulk::Compressor::with_dictionary(19, DICTIONARY)?;
    for batch in payloads.chunks(PAYLOAD_STEP) {
        if !more() {
            return Ok(result);
        }
        let mut moved = vec![];
        for (id, n, blob) in batch {
            let bytes = dec.decompress(blob, *n)?;
            ensure!(hash(&bytes) == *id, "model payload hash mismatch");
            let compact = enc.compress(&bytes)?;
            ensure!(dec.decompress(&compact, *n)? == bytes, "model decode mismatch");
            moved.push((id.clone(), compact));
        }
        result.payloads += queue.background(move |w| {
            let mut n = 0;
            for (id, compact) in &moved {
                n += w.execute(
                    "UPDATE payloads SET data=?2,codec='json-zstd-dict-v1-cold' WHERE id=?1 AND codec='json-zstd-dict-v1'",
                    params![id, compact],
                )?;
            }
            Ok(n)
        })?;
    }
    let images: Vec<(String, String, String, i64)> = db
        .prepare("SELECT id,mime,pack,wall_ns FROM images INDEXED BY image_age WHERE cold_checked=0 AND wall_ns<=?1 LIMIT 32")?
        .query_map([cutoff as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, mime, old_pack, wall) in images {
        if !more() {
            return Ok(result);
        }
        let old = {
            let _reader = reader_lock(root)?;
            records::image_bytes(&db, root, &id)?.1
        };
        // Keep the smaller representation; lossy image codecs cannot preserve training text exactly.
        // Bytes that do not decode as an image move cold unchanged.
        let webp = match image::load_from_memory(&old) {
            Ok(decoded) => {
                let img = decoded.to_rgba8();
                let mut webp = vec![];
                image::codecs::webp::WebPEncoder::new_lossless(&mut webp).encode(
                    &img,
                    img.width(),
                    img.height(),
                    image::ExtendedColorType::Rgba8,
                )?;
                ensure!(image::load_from_memory(&webp)?.to_rgba8() == img, "WebP pixel mismatch");
                Some(webp)
            }
            Err(_) => None,
        };
        let pack = store::month(wall.max(0) as u64);
        let (new_mime, bytes) = match webp {
            Some(webp) if webp.len() < old.len() => ("image/webp", webp),
            _ => (mime.as_str(), old),
        };
        let key = store::image_key(&id, new_mime)?;
        store::put(root, &pack, &[(key.clone(), &bytes)])?;
        let (r, new_mime, stored) = (root.to_path_buf(), new_mime.to_owned(), hash(&bytes));
        queue.background(move |w| {
            store::ensure_blob(&r, &pack, &key, &bytes)?;
            // Moved only if the catalogue still names the bytes read above.
            let still: bool = w.query_row("SELECT EXISTS(SELECT 1 FROM images WHERE id=?1 AND mime=?2 AND pack=?3 AND cold_checked=0)", params![id, mime, old_pack], |r| r.get(0))?;
            if !still {
                store::queue_gc(w, &pack, &key)?;
                return Ok(());
            }
            let old_key = store::image_key(&id, &mime)?;
            if (old_pack.as_str(), old_key.as_str()) != (pack.as_str(), key.as_str()) {
                store::queue_gc(w, &old_pack, &old_key)?;
            }
            w.execute(
                "UPDATE images SET mime=?2,pack=?3,stored_sha256=?4,cold_checked=1 WHERE id=?1",
                params![id, new_mime, pack, stored],
            )?;
            Ok(())
        })?;
        result.images += 1;
    }
    result.reclaimed_objects += collect_all(&queue, root, more)?;
    if result.images > 0 {
        store::reclaim_pack(root, store::HOT, 8192)?;
    }
    Ok(result)
}
pub fn compact_once(root: &Path, now: u64) -> Result<Receipt> {
    compact_pass(root, now, &mut || true)
}
/// One compaction pass over the store: legacy migration (one session), collection and the
/// hourly orphan sweep, up to `PASS_AUDIO_GROUPS` audio groups, `PASS_CHUNKS` chunks per kind,
/// then cold payloads and screenshots. `more()` is asked before each step; once it returns
/// false the pass ends after the step in progress.
pub fn compact_pass(root: &Path, now: u64, more: &mut dyn FnMut() -> bool) -> Result<Receipt> {
    let mut result = Receipt::default();
    if !more() {
        return Ok(result);
    }
    if crate::migrate::pending(root)? {
        let r = crate::migrate::migrate_next(root, &crate::migrate::Options::default());
        match r {
            Ok(Some(report)) if report.busy() => {}
            Ok(Some(report)) => {
                result.migrated_sessions += 1;
                if !report.verified {
                    result.errors.push(format!(
                        "migration of {} failed; folder kept: {}",
                        report.name,
                        report.error.as_deref().unwrap_or("unverified")
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => result.errors.push(format!("migration: {e:#}")),
        }
        if !more() {
            return Ok(result);
        }
    }
    if !store::exists(root) {
        return Ok(result);
    }
    if let Some(_owner) = owner(root)? {
        let queue = crate::live::Queue::shared(root)?;
        result.reclaimed_objects += collect_all(&queue, root, more)?;
        let last: u64 = store::open(root)?
            .query_row("SELECT value FROM store_meta WHERE key='sweep_wall'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if now.saturating_sub(last) >= store::ORPHAN_GRACE_NS {
            let r = root.to_path_buf();
            match queue.background(move |w| {
                let n = store::sweep(w, &r, now, store::COLLECT_KEYS)?;
                w.execute("INSERT INTO store_meta VALUES('sweep_wall',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [now.to_string()])?;
                Ok(n)
            }) {
                Ok(n) => result.reclaimed_objects += n,
                Err(e) => result.errors.push(format!("orphan sweep: {e:#}")),
            }
        }
    }
    let mut groups = budget(PASS_AUDIO_GROUPS);
    match compact_audio_while(root, now, &mut || groups() && more()) {
        Ok(r) => result.add(r),
        Err(e) => result.errors.push(format!("audio: {e:#}")),
    }
    #[cfg(target_os = "macos")]
    if maintenance_allowed() {
        match crate::video::tier_while(root, now, crate::video::PASS_EPOCHS, &mut || more() && maintenance_allowed()) {
            Ok(t) => {
                result.video_segments += t.tiered + t.retired;
                result.errors.extend(t.errors);
            }
            Err(e) => result.errors.push(format!("video: {e:#}")),
        }
    }
    for audio in [true, false] {
        if !more() {
            return Ok(result);
        }
        let mut chunks = budget(PASS_CHUNKS);
        match compact_metadata_while(root, now, audio, &mut || chunks() && more()) {
            Ok(n) => result.chunks += n,
            Err(e) => result.errors.push(format!("{} chunks: {e:#}", if audio { "timeline" } else { "evidence" })),
        }
    }
    if !more() {
        return Ok(result);
    }
    match compact_records_while(root, now, more) {
        Ok(r) => result.add(r),
        Err(e) => result.errors.push(format!("records: {e:#}")),
    }
    Ok(result)
}
/// Index catch-up and video tiering never inherit emergency audio admission.
fn maintenance_allowed()->bool {
    #[cfg(target_os="macos")]
    {crate::video::native::maintenance_allowed()}
    #[cfg(not(target_os="macos"))]
    {false}
}
/// Recheck AC + idle at every cancellation boundary, including before decoding
/// and committing a recording projection. The audio gate remains separate.
pub fn backfill_chunks_while(root:&Path,more:&mut impl FnMut()->bool,maintenance:&mut impl FnMut()->bool)->Result<usize> {
    crate::timeline::backfill(root,&mut || more() && maintenance())
}
/// How often a running pass re-asks the caller's gate between steps.
pub const GATE_RECHECK: Duration = Duration::from_secs(1);
/// While the gate is closed, how often it is asked again.
pub const GATE_WAIT: Duration = Duration::from_secs(30);
/// Event-driven stop; one low-priority thread.
pub struct Compactor {
    stop: Arc<(Mutex<bool>, Condvar)>,
    /// Set by `preempt`: the step in flight stops at its next statement (interrupted, rolled back) and the loop ends.
    preempted: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    status: Arc<Mutex<Option<String>>>,
}
/// The compactors running in this process (`preempt_all`).
fn running() -> &'static Mutex<Vec<Weak<AtomicBool>>> {
    static RUNNING: std::sync::OnceLock<Mutex<Vec<Weak<AtomicBool>>>> = std::sync::OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(vec![]))
}
/// SQLite's "interrupted" (code 9): what a step stopped by `preempt` fails with.
pub(crate) fn interrupted(error: &str) -> bool {
    error.contains("(Error code 9)") || error.contains("sqlite3_interrupt") || error.trim_end().ends_with("interrupted")
}
/// A pass's health status and log line from its errors (none: none). A step interrupted because the compactor was
/// stopped (Quit, `preempt_all`) fails with SQLite's "interrupted" (code 9) and rolls back: that is the stop, not a
/// delay (night 1027: all three "Error code 9" lines and their "delayed" status came from Quit during installs). Any
/// other error of that pass keeps its text and its delay (review, night 1027).
fn outcome(errors: &[String], stopped: bool) -> (Option<String>, Option<String>) {
    let rest: Vec<&str> = errors.iter().map(String::as_str).filter(|e| !(stopped && interrupted(e))).collect();
    match (errors.is_empty(), rest.is_empty()) {
        (true, _) => (None, None),
        (false, true) => (None, Some("archive compaction stopped for Quit; its step rolled back and runs again next launch".into())),
        (false, false) => (Some("Archive compaction delayed".into()), Some(format!("archive compaction retained originals: {}", rest.join("; ")))),
    }
}
/// Quit: every compactor in this process stops now. Its preparation's statements are interrupted; a writer job in
/// flight is one bounded transaction and finishes or rolls back, so nothing is lost and the next launch goes on.
pub fn preempt_all() {
    let running = running().lock().unwrap();
    for p in running.iter().filter_map(Weak::upgrade) {
        p.store(true, Ordering::Release);
    }
}
impl Compactor {
    /// Stops this compactor now (see `preempt_all`); it does not run again.
    pub fn preempt(&self) {
        self.preempted.store(true, Ordering::Release);
        *self.stop.0.lock().unwrap() = true;
        self.stop.1.notify_all();
    }
    pub fn status(&self) -> Option<String> {
        self.status.lock().unwrap().clone()
    }
    pub fn start(root: PathBuf) -> Result<Self> {
        Self::start_gated(root, || true)
    }
    /// `gate()` says whether compaction may use energy now (the app: on AC power, or
    /// free disk near its floor). It is asked before each pass, at most every
    /// `GATE_RECHECK` inside a pass, and every `GATE_WAIT` while closed. A closed gate
    /// stops the pass after the group or chunk in progress.
    pub fn start_gated(root: PathBuf, gate: impl FnMut() -> bool + Send + 'static) -> Result<Self> {
        Self::start_with(root, gate, GATE_RECHECK, GATE_WAIT)
    }
    #[doc(hidden)]
    pub fn start_with(
        root: PathBuf,
        mut gate: impl FnMut() -> bool + Send + 'static,
        recheck: Duration,
        wait: Duration,
    ) -> Result<Self> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let state = stop.clone();
        let preempt = Arc::new(AtomicBool::new(false));
        let preempted = preempt.clone();
        {
            let mut running = running().lock().unwrap();
            running.retain(|p| p.strong_count() > 0);
            running.push(Arc::downgrade(&preempted));
        }
        let status = Arc::new(Mutex::new(None));
        let reported = status.clone();
        let thread = std::thread::Builder::new()
            .name("heimdall-cold-archive".into())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
                }
                store::interrupt_when(preempt.clone());
                let mut last_error = None::<std::time::Instant>;
                loop {
                    if *state.0.lock().unwrap() || preempt.load(Ordering::Acquire) {
                        break;
                    }
                    let delay = if !gate() {
                        wait
                    } else {
                        let mut asked = std::time::Instant::now();
                        let mut open = true;
                        let mut more = || {
                            if *state.0.lock().unwrap() || preempt.load(Ordering::Acquire) {
                                return false;
                            }
                            if open && asked.elapsed() >= recheck {
                                open = gate();
                                asked = std::time::Instant::now();
                            }
                            open
                        };
                        match compact_pass(&root, wall_ns(), &mut more) {
                            Ok(r) => {
                                // Cold audio keeps its existing admission/priority. Index catch-up
                                // additionally requires idle AC; emergency audio admission cannot open it.
                                let mut remaining=256usize;
                                let indexed=if more() {
                                    match backfill_chunks_while(&root,&mut || {
                                        if remaining==0 || !more() {return false;}
                                        remaining-=1; true
                                    }, &mut maintenance_allowed) {Ok(n)=>n,Err(e)=>{eprintln!("timeline backfill delayed: {e:#}");0}}
                                } else {0};
                                let stopped = *state.0.lock().unwrap() || preempt.load(Ordering::Acquire);
                                let (status, line) = outcome(&r.errors, stopped);
                                *reported.lock().unwrap() = status;
                                if let Some(line) = line.filter(|_| stopped || last_error.is_none_or(|t| t.elapsed() >= Duration::from_secs(60))) {
                                    eprintln!("{line}");
                                    last_error = Some(std::time::Instant::now());
                                }
                                if !open {
                                    wait
                                } else if r.worked() || indexed>0 {
                                    Duration::from_millis(250)
                                } else if !r.errors.is_empty() {
                                    Duration::from_secs(60)
                                } else {
                                    Duration::from_secs(300)
                                }
                            }
                            Err(e) => {
                                let stopped = *state.0.lock().unwrap() || preempt.load(Ordering::Acquire);
                                let (status, line) = outcome(&[format!("{e:#}")], stopped);
                                *reported.lock().unwrap() = status;
                                if let Some(line) = line { eprintln!("{line}") }
                                Duration::from_secs(60)
                            }
                        }
                    };
                    let guard = state.0.lock().unwrap();
                    if *guard {
                        break;
                    }
                    let _ = state.1.wait_timeout(guard, delay).unwrap();
                }
                store::close_packs();
            })?;
        Ok(Self { stop, preempted, thread: Some(thread), status })
    }
}
impl Drop for Compactor {
    fn drop(&mut self) {
        // Shutdown does not wait for the step in flight.
        self.preempt();
        // Unregistered before the join frees the thread `preempt_all` would boost.
        running().lock().unwrap().retain(|p| p.upgrade().is_some_and(|p| !Arc::ptr_eq(&p, &self.preempted)));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Packs old rows into chunks: one chunk now.
pub fn compact_metadata(root: &Path, now: u64, audio: bool) -> Result<usize> {
    compact_metadata_while(root, now, audio, &mut budget(1))
}
/// Packs chunks while `more()` allows and eligible rows remain. Returns chunks packed.
pub fn compact_metadata_while(root: &Path, now: u64, audio: bool, more: &mut dyn FnMut() -> bool) -> Result<usize> {
    let Some(_owner) = owner(root)? else {
        return Ok(0);
    };
    let queue = crate::live::Queue::shared(root)?;
    let db = store::open(root)?;
    // A chunk's build tables (a thousand rows and their bodies) stay in memory: on disk every temp page was another
    // throttled background I/O.
    db.execute_batch("PRAGMA temp_store=MEMORY")?;
    let cutoff = now.saturating_sub(DAY_NS);
    let mut n = 0;
    while more() {
        let packed = if audio {
            chunks::pack_blocks(&db, &queue, root, cutoff)?
        } else {
            chunks::pack_evidence(&db, &queue, root, cutoff)?
        };
        if packed == 0 {
            break;
        }
        n += 1;
    }
    if n > 0 {
        reclaim_index(&queue, 2048 * n as u32, more)?;
    }
    Ok(n)
}

#[cfg(test)]
mod reader_tests {
    /// A stall line names the thread holding the archive, and forgets it once the lock is dropped.
    #[test]
    fn readers_held_names_the_holding_thread() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path().to_path_buf();
        let (held, release) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<()>());
        let (held_tx, held_rx) = held;
        let (release_tx, release_rx) = release;
        let thread = std::thread::Builder::new()
            .name("test-archive-reader".into())
            .spawn(move || {
                let _lock = super::reader_lock(&r).unwrap();
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
        held_rx.recv().unwrap();
        assert!(super::readers_held().contains("test-archive-reader for "), "{}", super::readers_held());
        release_tx.send(()).unwrap();
        thread.join().unwrap();
        assert!(!super::readers_held().contains("test-archive-reader"), "{}", super::readers_held());
    }
}
#[cfg(test)]
mod outcome_tests {
    #[test]
    fn a_step_interrupted_by_quit_is_no_delay() {
        let e = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(super::outcome(&[], false), (None, None));
        assert_eq!(super::outcome(&[], true), (None, None));
        let (status, line) = super::outcome(&e(&["interrupted (Error code 9)"]), true);
        assert_eq!(status, None, "Quit is not a compaction delay");
        assert!(line.unwrap().contains("stopped for Quit"));
        let (status, line) = super::outcome(&e(&["pack 2026-10: interrupted"]), true);
        assert_eq!((status, line.unwrap().contains("stopped for Quit")), (None, true));
        let (status, line) = super::outcome(&e(&["database is locked"]), false);
        assert_eq!(status.as_deref(), Some("Archive compaction delayed"));
        assert!(line.unwrap().contains("database is locked"));
    }
    /// Review of night 1027: on a stop, every error of that pass became the Quit line. Only interrupts do.
    #[test]
    fn other_errors_of_a_stopped_pass_keep_their_text() {
        let e = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (status, line) = super::outcome(&e(&["disk I/O error (Error code 10)", "interrupted (Error code 9)"]), true);
        assert_eq!(status.as_deref(), Some("Archive compaction delayed"));
        let line = line.unwrap();
        assert!(line.contains("disk I/O error") && !line.contains("Error code 9"), "{line}");
        let (status, line) = super::outcome(&e(&["database disk image is malformed"]), true);
        assert_eq!(status.as_deref(), Some("Archive compaction delayed"));
        assert!(line.unwrap().contains("malformed"));
    }
}
