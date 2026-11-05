use crate::{hash, opus, store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}
pub fn pcm_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|x| x.to_bits().to_le_bytes())
        .collect()
}
pub fn pcm_from_bytes(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(bytes.len().is_multiple_of(4), "partial PCM sample");
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|x| f32::from_bits(u32::from_le_bytes(*x)))
        .collect())
}
/// One encoded audio object, ready to be committed to a pack and catalogued.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub id: String,
    pub rate: u32,
    pub channels: u16,
    pub frames: i64,
    pub codec: &'static str,
    pub bytes: Vec<u8>,
    pub stored_sha256: String,
}
/// Content identity of native PCM: rate + channels + float bits. Source/time are references.
pub fn native_id(rate: u32, channels: u16, pcm: &[f32]) -> String {
    let mut identity = rate.to_le_bytes().to_vec();
    identity.extend(channels.to_le_bytes());
    identity.extend(pcm_bytes(pcm));
    hash(&identity)
}
/// Lossless native encoding: planar channels and byte planes expose float exponent and
/// repeated-channel structure to zstd. A bit permutation, never PCM quantization.
pub fn encode_lossless(rate: u32, channels: u16, pcm: &[f32]) -> Result<Encoded> {
    ensure!(
        rate > 0 && channels > 0 && pcm.len().is_multiple_of(channels as usize),
        "invalid native PCM"
    );
    let frames = pcm.len() / channels as usize;
    let mut shuffled = vec![0u8; pcm.len() * 4];
    for (i, sample) in pcm.iter().enumerate() {
        let frame = i / channels as usize;
        let channel = i % channels as usize;
        for (plane, byte) in sample.to_bits().to_le_bytes().into_iter().enumerate() {
            shuffled[(channel * 4 + plane) * frames + frame] = byte;
        }
    }
    let bytes = zstd::bulk::compress(&shuffled, 1)?;
    Ok(Encoded {
        id: native_id(rate, channels, pcm),
        rate,
        channels,
        frames: frames as i64,
        codec: "f32-zstd-planar-v1",
        stored_sha256: hash(&bytes),
        bytes,
    })
}
pub fn catalogued(db: &Connection, id: &str) -> Result<bool> {
    Ok(db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM main.audio_objects WHERE id=?1)")?
        .query_row([id], |r| r.get(0))?)
}
/// Catalogues objects whose blobs were committed to `pack` beforehand. Runs inside the caller's
/// index write transaction (or opens one): each blob is re-checked under the write lock and
/// rewritten if the collector removed it in between. An already catalogued ID is left as is.
pub fn publish(db: &Connection, root: &Path, pack: &str, objects: &[Encoded]) -> Result<()> {
    if db.is_autocommit() && !crate::live::on_writer() {
        // Not inside a writer job: the store's writer catalogues them (and the caller waits for it).
        let (root, pack, objects) = (root.to_path_buf(), pack.to_owned(), objects.to_vec());
        return store::written(db, move |db| crate::timeline::transaction(db, || publish(db, &root, &pack, &objects)));
    }
    for o in objects {
        if catalogued(db, &o.id)? {
            continue;
        }
        store::ensure_blob(root, pack, &store::audio_key(&o.id), &o.bytes)?;
        insert_object(db, pack, o)?;
    }
    Ok(())
}
fn insert_object(db: &Connection, pack: &str, o: &Encoded) -> Result<()> {
    db.prepare_cached("INSERT INTO main.audio_objects(id,rate,channels,frames,codec,pack,stored_sha256) VALUES(?1,?2,?3,?4,?5,?6,?7)")?
        .execute(params![o.id, o.rate, o.channels, o.frames, o.codec, pack, o.stored_sha256])?;
    Ok(())
}
/// `publish` for objects whose blobs the caller committed to `pack` after reading `collections`
/// (`store::collections`), inside the writer's transaction. While no collection has removed a blob since, they are
/// still there and are catalogued without reading the pack under the write lock, where the read can wait for a pooled
/// pack connection and the pack's own full-sync commits and checkpoints while every index write waits behind it
/// (capture's flush, `crates/capture/src/archive.rs:297`, held the writer 2-5 s at a time on 4 Nov 22:00-22:15 and 5
/// Nov 00:10-00:13). Otherwise it is `publish`.
pub fn publish_put(db: &Connection, root: &Path, pack: &str, objects: &[Encoded], collections: i64) -> Result<()> {
    if db.is_autocommit() || store::collections(db)? != collections {
        return publish(db, root, pack, objects);
    }
    for o in objects {
        if catalogued(db, &o.id)? {
            continue;
        }
        insert_object(db, pack, o)?;
    }
    Ok(())
}
/// Native PCM as a lossless object in `pack` (the hot pack for capture). Returns its ID.
pub fn put_lossless(db: &Connection, root: &Path, pack: &str, rate: u32, channels: u16, pcm: &[f32]) -> Result<String> {
    let id = native_id(rate, channels, pcm);
    if catalogued(db, &id)? {
        return Ok(id);
    }
    let object = encode_lossless(rate, channels, pcm)?;
    store::put(root, pack, &[(store::audio_key(&object.id), &object.bytes)])?;
    publish(db, root, pack, std::slice::from_ref(&object))?;
    Ok(id)
}
/// Hot native object (capture's one-second batches, tests).
pub fn put_hot(db: &Connection, rate: u32, channels: u16, pcm: &[f32]) -> Result<String> {
    let root = store::root_of(db)?;
    put_lossless(db, &root, store::HOT, rate, channels, pcm)
}
/// Stored bytes of a catalogued object, checksum-verified. A concurrent collector can move a hot
/// object between the catalogue read and the blob read; the catalogue is then read again.
pub fn object_bytes(db: &Connection, root: &Path, id: &str) -> Result<(u32, u16, usize, String, Vec<u8>)> {
    let mut attempt = 0;
    loop {
        let (rate, channels, frames, codec, pack, digest) = db
            .prepare_cached("SELECT rate,channels,frames,codec,pack,stored_sha256 FROM main.audio_objects WHERE id=?1")?
            .query_row([id], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, u16>(1)?,
                    r.get::<_, i64>(2)? as usize,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .with_context(|| format!("audio object {id} is not catalogued"))?;
        match store::get(root, &pack, &store::audio_key(id))? {
            Some(bytes) => {
                ensure!(hash(&bytes) == digest, "object hash mismatch");
                return Ok((rate, channels, frames, codec, bytes));
            }
            None if attempt < 2 => attempt += 1,
            None => anyhow::bail!("audio object {id} missing from pack {pack}"),
        }
    }
}
pub fn decode_object(rate: u32, channels: u16, frames: usize, codec: &str, bytes: &[u8]) -> Result<Pcm> {
    ensure!(
        channels > 0 && frames <= rate as usize * 120,
        "object geometry exceeds two-minute bound"
    );
    let samples = match codec {
        "f32-zstd-planar-v1" => {
            let shuffled = zstd::bulk::decompress(bytes, frames * channels as usize * 4)?;
            ensure!(shuffled.len() == frames * channels as usize * 4, "shuffled PCM extent");
            let mut values = Vec::with_capacity(frames * channels as usize);
            for frame in 0..frames {
                for channel in 0..channels as usize {
                    let bits = std::array::from_fn(|plane| shuffled[(channel * 4 + plane) * frames + frame]);
                    values.push(f32::from_bits(u32::from_le_bytes(bits)));
                }
            }
            values
        }
        "f32-zstd" => pcm_from_bytes(&zstd::bulk::decompress(bytes, frames * channels as usize * 4)?)?,
        "opus-har1" => decode_opus(bytes)?,
        "flac16" => decode_flac(bytes, frames)?,
        _ => anyhow::bail!("unknown audio codec {codec}"),
    };
    ensure!(samples.len() == frames * channels as usize, "decoded length mismatch");
    Ok(Pcm { rate, channels, samples })
}
pub fn read_object(db: &Connection, id: &str) -> Result<Pcm> {
    let root = store::root_of(db)?;
    let (rate, channels, frames, codec, bytes) = object_bytes(db, &root, id)?;
    decode_object(rate, channels, frames, &codec, &bytes)
}
pub fn read_block(db: &Connection, id: i64) -> Result<Pcm> {
    read_block_cached(db, id, &mut ObjectCache::default())
}
/// The most recently decoded object. Hot objects hold about one second of callbacks, so
/// consecutive blocks share one; IDs are content hashes, so a cached entry cannot go stale.
#[derive(Default)]
pub struct ObjectCache(Option<(String, Pcm)>);
impl ObjectCache {
    fn get(&mut self, db: &Connection, id: &str) -> Result<&Pcm> {
        if self.0.as_ref().is_none_or(|(cached, _)| cached != id) {
            self.0 = Some((id.to_owned(), read_object(db, id)?));
        }
        Ok(&self.0.as_ref().unwrap().1)
    }
}
/// One timeline row's geometry and object reference.
#[derive(Debug, Clone)]
pub struct BlockRef {
    pub rate: u32,
    pub channels: u16,
    pub frames: usize,
    pub codec: String,
    pub object: Option<String>,
    pub offset: usize,
    pub gain: f32,
}
fn block_ref(db: &Connection, id: i64) -> Result<BlockRef> {
    // `blocks` is the session view's temporary table when present, else the index; a packed row
    // is read from its chunk.
    let live = db
        .prepare_cached("SELECT rate,channels,frames,codec,object_id,offset_frames,gain FROM blocks WHERE id=?1")?
        .query_row([id], |r| {
            Ok(BlockRef {
                rate: r.get(0)?,
                channels: r.get(1)?,
                frames: r.get::<_, i64>(2)? as usize,
                codec: r.get(3)?,
                object: r.get(4)?,
                offset: r.get::<_, Option<i64>>(5)?.unwrap_or(0) as usize,
                gain: r.get::<_, Option<f64>>(6)?.unwrap_or(1.) as f32,
            })
        })
        .optional()?;
    if let Some(b) = live {
        return Ok(b);
    }
    let root = store::root_of(db)?;
    let row = crate::chunks::find_block(db, &root, id)?.with_context(|| format!("block {id} not in the archive"))?;
    let get = |name: &str| row.iter().find(|(c, _)| c == name).map(|(_, v)| v.clone()).unwrap_or(Value::Null);
    let int = |name: &str| match get(name) {
        Value::Integer(n) => Ok(n),
        other => anyhow::bail!("block {id} column {name}: {other:?}"),
    };
    Ok(BlockRef {
        rate: int("rate")? as u32,
        channels: int("channels")? as u16,
        frames: int("frames")? as usize,
        codec: match get("codec") {
            Value::Text(s) => s,
            _ => anyhow::bail!("block {id} codec"),
        },
        object: match get("object_id") {
            Value::Text(s) => Some(s),
            _ => None,
        },
        offset: int("offset_frames").unwrap_or(0) as usize,
        gain: match get("gain") {
            Value::Real(g) => g as f32,
            Value::Integer(g) => g as f32,
            _ => 1.,
        },
    })
}
/// `read_block` that decodes and verifies each referenced object once per run of blocks.
pub fn read_block_cached(db: &Connection, id: i64, cache: &mut ObjectCache) -> Result<Pcm> {
    let b = block_ref(db, id)?;
    if b.codec == "zero" {
        return Ok(Pcm {
            rate: b.rate,
            channels: b.channels,
            samples: vec![0.; b.frames.checked_mul(b.channels as usize).context("silence length overflow")?],
        });
    }
    let object = b.object.with_context(|| format!("block {id} has no audio object"))?;
    let p = cache.get(db, &object)?;
    // Original frame positions remain immutable; decoded PCM geometry is explicit.
    let a = b.offset as u64 * p.rate as u64 / b.rate as u64;
    let e = (b.offset + b.frames) as u64 * p.rate as u64 / b.rate as u64;
    let a = a as usize * p.channels as usize;
    let e = e as usize * p.channels as usize;
    ensure!(e <= p.samples.len(), "block exceeds object");
    let samples = if b.gain == 1. {
        p.samples[a..e].to_vec()
    } else {
        p.samples[a..e].iter().map(|x| x * b.gain).collect()
    };
    Ok(Pcm { rate: p.rate, channels: p.channels, samples })
}
pub fn mono_16k(samples: &[f32], rate: u32, channels: u16) -> Result<Vec<f32>> {
    use rubato::{FftFixedInOut, Resampler};
    ensure!(
        rate > 0 && channels > 0 && samples.len().is_multiple_of(channels as usize),
        "invalid resampling input"
    );
    let mono: Vec<_> = samples
        .chunks_exact(channels as usize)
        .map(|c| c.iter().sum::<f32>() / channels as f32)
        .collect();
    if rate == 16000 {
        return Ok(mono);
    }
    if mono.is_empty() {
        return Ok(vec![]);
    }
    let mut r = FftFixedInOut::<f32>::new(rate as usize, 16000, 1024, 1)?;
    let step = r.input_frames_next();
    let delay = r.output_delay();
    let wanted = mono.len() * 16000 / rate as usize;
    let mut out = vec![];
    for c in mono.chunks(step) {
        let mut b = vec![0.; step];
        b[..c.len()].copy_from_slice(c);
        out.extend(r.process(&[b], None)?.remove(0));
    }
    while out.len() < wanted + delay {
        out.extend(r.process(&[vec![0.; step]], None)?.remove(0));
    }
    Ok(out[delay..delay + wanted].to_vec())
}
/// Self-contained versioned Opus packets. lookahead and zero-padded tail are removed on decode.
pub fn encode_opus(pcm: &[f32], bitrate: i32) -> Result<Vec<u8>> {
    encode_opus_mode(pcm, bitrate, false)
}
pub fn encode_opus_mode(pcm: &[f32], bitrate: i32, speech: bool) -> Result<Vec<u8>> {
    ensure!(
        !pcm.is_empty()
            && pcm.len() <= 16000 * 120
            && pcm.iter().all(|x| x.is_finite())
            && (6000..=128000).contains(&bitrate),
        "invalid Opus input"
    );
    let mut enc = opus::Encoder::configured(bitrate, speech)?;

    let look = enc.get_lookahead()? as usize;
    let mut out = b"HAR1".to_vec();
    out.extend((pcm.len() as u32).to_le_bytes());
    out.extend((look as u32).to_le_bytes());
    out.extend(bitrate.to_le_bytes());
    for offset in (0..pcm.len() + look).step_by(320) {
        let mut block = [0.; 320];
        let n = pcm.len().saturating_sub(offset).min(320);
        if n > 0 {
            block[..n].copy_from_slice(&pcm[offset..offset + n]);
        }
        let mut packet = [0; 1275];
        let n = enc.encode_float(&block, &mut packet)?;
        out.extend((n as u16).to_le_bytes());
        out.extend(&packet[..n]);
    }
    Ok(out)
}
pub fn decode_opus(bytes: &[u8]) -> Result<Vec<f32>> {
    ensure!(
        bytes.len() >= 16 && &bytes[..4] == b"HAR1",
        "invalid Opus archive header"
    );
    let n = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let look = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    ensure!(n <= 16000 * 120 && look < 16000, "Opus archive too large");
    let mut dec = opus::Decoder::new()?;
    let mut pos = 16;
    let mut out = vec![];
    while pos < bytes.len() {
        ensure!(pos + 2 <= bytes.len(), "truncated packet length");
        let size = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap()) as usize;
        pos += 2;
        ensure!(
            size > 0 && pos + size <= bytes.len() && out.len() < n + look + 320,
            "invalid packet extent"
        );
        let mut block = [0.; 320];
        let got = dec.decode_float(&bytes[pos..pos + size], &mut block)?;
        ensure!(got == 320, "unexpected Opus frame length");
        out.extend(block);
        pos += size;
    }
    ensure!(
        out.len() == (n + look).div_ceil(320) * 320,
        "missing Opus tail"
    );
    Ok(out[look..look + n].to_vec())
}
pub fn encode_flac(pcm: &[f32]) -> Result<Vec<u8>> {
    use flacenc::{component::BitRepr, error::Verify};
    ensure!(
        !pcm.is_empty() && pcm.iter().all(|x| x.is_finite()),
        "invalid FLAC speech"
    );
    let mut samples: Vec<i32> = pcm
        .iter()
        .map(|x| (x.clamp(-1., 1.) * 32768.).round().clamp(-32768., 32767.) as i32)
        .collect();
    samples.resize(samples.len().max(16), 0);
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| anyhow::anyhow!("FLAC config: {e}"))?;
    let source = flacenc::source::MemSource::from_samples(&samples, 1, 16, 16000);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|e| anyhow::anyhow!("FLAC encode: {e}"))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| anyhow::anyhow!("FLAC write: {e}"))?;
    Ok(sink.as_slice().to_vec())
}
pub fn decode_flac(bytes: &[u8], samples: usize) -> Result<Vec<f32>> {
    let mut reader = claxon::FlacReader::new(bytes)?;
    ensure!(
        reader.streaminfo().channels == 1
            && reader.streaminfo().sample_rate == 16000
            && reader.streaminfo().bits_per_sample == 16,
        "invalid cold FLAC geometry"
    );
    let values = reader
        .samples()
        .map(|x| x.map(|v| v as f32 / 32768.))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(values.len() == samples.max(16), "cold FLAC length mismatch");
    Ok(values[..samples].to_vec())
}

#[cfg(test)]
mod publish_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Capture's flush (`crates/capture/src/archive.rs:297`) held the writer 2-5 s at a time (4 Nov 22:00-22:15, 5 Nov
    /// 00:10-00:13) while every index write waited. Its blobs are committed before its job, so under the write lock it
    /// no longer reads the hot pack: with every pooled hot-pack connection lent out (a pack commit or checkpoint
    /// running), the job still commits at once.
    #[test]
    fn publishing_blobs_written_beforehand_never_waits_for_the_pack() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let queue = crate::live::Queue::shared(&root).unwrap();
        let pcm: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.02).sin() * 0.2).collect();
        let object = encode_lossless(16_000, 1, &pcm).unwrap();
        let collections = store::collections(&store::open(&root).unwrap()).unwrap();
        store::put(&root, store::HOT, &[(store::audio_key(&object.id), &object.bytes)]).unwrap();

        let (lent, release) = (std::sync::mpsc::channel::<()>(), std::sync::mpsc::channel::<()>());
        let pack_root = root.clone();
        let holder = std::thread::spawn(move || {
            fn hold(root: &Path, left: usize, lent: &std::sync::mpsc::Sender<()>, release: &std::sync::mpsc::Receiver<()>) -> Result<()> {
                if left == 0 {
                    lent.send(()).unwrap();
                    release.recv().unwrap();
                    return Ok(());
                }
                store::with_pack(root, store::HOT, |_| hold(root, left - 1, lent, release))
            }
            hold(&pack_root, store::PACK_CONNECTIONS, &lent.0, &release.1).unwrap();
        });
        lent.1.recv().unwrap();
        let started = Instant::now();
        let (r, o) = (root.clone(), object.clone());
        let done = std::thread::spawn(move || queue.call(move |db| publish_put(db, &r, store::HOT, std::slice::from_ref(&o), collections)));
        while !done.is_finished() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let waited = started.elapsed();
        release.0.send(()).unwrap();
        holder.join().unwrap();
        done.join().unwrap().unwrap();
        assert!(waited < Duration::from_secs(1), "the publishing job waited {waited:?} for the hot pack");
        assert!(catalogued(&store::open(&root).unwrap(), &object.id).unwrap());
    }

    /// A collection between writing the blob and publishing it (identical audio returning while its old blob is
    /// collected) changes the count: the blob is checked under the write lock and written again.
    #[test]
    fn a_collection_in_between_rewrites_the_blob() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let queue = crate::live::Queue::shared(&root).unwrap();
        let pcm: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.03).cos() * 0.2).collect();
        let object = encode_lossless(16_000, 1, &pcm).unwrap();
        let key = store::audio_key(&object.id);
        let collections = store::collections(&store::open(&root).unwrap()).unwrap();
        store::put(&root, store::HOT, &[(key.clone(), &object.bytes)]).unwrap();
        let (r, k) = (root.clone(), key.clone());
        let removed = queue
            .call(move |db| {
                store::queue_gc(db, store::HOT, &k)?;
                store::collect(db, &r, store::COLLECT_KEYS)
            })
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!store::has(&root, store::HOT, &key).unwrap());
        let (r, o) = (root.clone(), object.clone());
        queue.call(move |db| publish_put(db, &r, store::HOT, std::slice::from_ref(&o), collections)).unwrap();
        assert!(store::has(&root, store::HOT, &key).unwrap(), "rewritten");
        assert_eq!(store::collections(&store::open(&root).unwrap()).unwrap(), collections + 1);
    }
}
