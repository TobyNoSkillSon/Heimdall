//! Validate an offline Jarvis bank completely before opening a destination writer.
//! Reads use private SQLite copies (including WAL), never source/profile sidecars.
//! Writes only use Store::insert, under the archive's shared lifetime lock.
use crate::{ClipMeta, LineSet, Origin, Store};
use anyhow::{ensure, Context, Result};
use heimdall_archive::{audio, compact, hash, store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MAX_PCM_BYTES: usize = 512 * 1024 * 1024;
pub const DEFAULT_FREE_SPACE_FLOOR_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const SQLITE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// Do not project a path across a nonexistent component followed by `..`.
/// Both fill and import sources must remain outside the installed profile.
pub fn ensure_lab_path(path: &Path) -> Result<()> {
    ensure!(
        !path
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "parent-directory (..) components are not allowed in lab paths"
    );
    // The profile in use and the personal edition's old one (a link to the new one after a move).
    let profiles = [heimdall_home::data_dir(), heimdall_home::Env::current().legacy_data()]
        .map(|p| p.canonicalize().unwrap_or(p));
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let ancestor = absolute
        .ancestors()
        .find(|p| p.exists())
        .context("lab path has no existing parent")?;
    let resolved = ancestor
        .canonicalize()?
        .join(absolute.strip_prefix(ancestor)?);
    ensure!(
        !path
            .to_string_lossy()
            .contains("Application Support/Heimdall")
            && !profiles.iter().any(|p| resolved.starts_with(p)),
        "refusing Heimdall's own data directory; use a lab directory"
    );
    Ok(())
}

struct DiskGuard {
    data: PathBuf,
}
impl DiskGuard {
    fn floor(&self) -> Result<u64> {
        let bytes = match std::fs::read(self.data.join("settings.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DEFAULT_FREE_SPACE_FLOOR_BYTES)
            }
            Err(e) => return Err(e.into()),
        };
        let settings: Value =
            serde_json::from_slice(&bytes).context("read-only import settings")?;
        ensure!(settings.is_object(), "settings.json must be an object");
        let floor = match settings.get("free_space_floor_bytes") {
            Some(value) => value.as_u64().context("invalid free_space_floor_bytes")?,
            None => DEFAULT_FREE_SPACE_FLOOR_BYTES,
        };
        ensure!(
            floor >= 1024 * 1024 * 1024,
            "free-space floor must be at least 1 GiB"
        );
        Ok(floor)
    }
    fn check(&self, path: &Path, allocation: u64) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        // Checked arithmetic makes u64::MAX refuse before any allocation.
        let required = self
            .floor()?
            .checked_add(SQLITE_MARGIN_BYTES)
            .and_then(|n| n.checked_add(allocation))
            .context("free-space floor reservation overflow")?;
        let ancestor = path
            .ancestors()
            .find(|p| p.exists())
            .context("disk check has no existing parent")?;
        let c = std::ffi::CString::new(ancestor.as_os_str().as_bytes())?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::statvfs(c.as_ptr(), &mut stat) } == 0,
            "disk free-space check: {}",
            std::io::Error::last_os_error()
        );
        let available = (stat.f_bavail as u64)
            .checked_mul(stat.f_frsize as u64)
            .context("disk size overflow")?;
        ensure!(
            available >= required,
            "free-space floor: {available} bytes available, {required} required at {}",
            ancestor.display()
        );
        Ok(())
    }
    fn snapshot(&self, allocation: u64) -> Result<()> {
        self.check(&self.data, 0)?;
        self.check(&std::env::temp_dir(), allocation)
    }
}

fn insert_reserve(pcm_bytes: u64, clips: usize) -> Result<u64> {
    // Four PCM extents plus per-clip metadata cover the encoded pack, its WAL
    // and checkpoint growth. check() additionally retains a fixed SQLite margin.
    pcm_bytes
        .checked_add(
            (clips as u64)
                .checked_mul(64 * 1024)
                .context("clip reserve overflow")?,
        )
        .and_then(|n| n.checked_mul(4))
        .context("import disk reservation overflow")
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub lines: usize,
    /// Of `lines`, how many are instant acknowledgements (the ack set, English and Polish).
    pub acks: usize,
    pub validated: usize,
    pub already_present: usize,
    pub would_insert: usize,
    pub inserted: usize,
    pub concurrent_safe: bool,
    pub lines_warning: Option<String>,
}

/// Copy a SQLite database and its committed WAL into private scratch. Never
/// open the original with SQLite: even READ_ONLY can write shared-memory files.
/// A changing source fails closed instead of accepting a torn file copy.
struct Snapshot {
    db: Connection,
    _scratch: tempfile::TempDir,
}
#[cfg(unix)]
fn stamp(path: &Path) -> Result<Option<(u64, u64, i64, i64, i64, i64)>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(s) => Ok(Some((
            s.ino(),
            s.len(),
            s.mtime(),
            s.mtime_nsec(),
            s.ctime(),
            s.ctime_nsec(),
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
impl Snapshot {
    fn open(path: &Path, disk: &DiskGuard) -> Result<Self> {
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        for _ in 0..3 {
            let before = (stamp(path)?, stamp(&wal)?);
            ensure!(
                before.0.is_some(),
                "missing archive database {}",
                path.display()
            );
            let allocation = before
                .0
                .unwrap()
                .1
                .checked_add(
                    before
                        .1
                        .map_or(0, |s| s.1)
                        .checked_mul(2)
                        .context("snapshot WAL size overflow")?,
                )
                .context("snapshot size overflow")?;
            // Reserve the DB copy, WAL copy and possible checkpoint growth
            // before even creating the scratch directory.
            disk.snapshot(allocation)?;
            let scratch = tempfile::tempdir()?;
            let copy = scratch.path().join("snapshot.sqlite");
            std::fs::copy(path, &copy)?;
            if before.1.is_some() {
                if std::fs::copy(&wal, scratch.path().join("snapshot.sqlite-wal")).is_err() {
                    continue;
                }
            }
            if before != (stamp(path)?, stamp(&wal)?) {
                continue;
            }
            disk.snapshot(0)?;
            // A read-only scratch connection still reads the copied WAL and
            // creates only private SHM; it cannot checkpoint/grow the DB on drop.
            let db =
                Connection::open_with_flags(&copy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.execute_batch("PRAGMA query_only=ON;")?;
            ensure!(
                db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? == "ok",
                "invalid archive database {}",
                path.display()
            );
            return Ok(Self {
                db,
                _scratch: scratch,
            });
        }
        anyhow::bail!(
            "archive database changed during snapshot; retry import: {}",
            path.display()
        )
    }
}

fn words(text: &str) -> Vec<String> {
    let text = text.replace('’', "'").to_lowercase();
    let mut chars = text.chars().peekable();
    let mut result = Vec::new();
    let mut word = String::new();
    let mut apostrophe = false;
    while let Some(c) = chars.next() {
        let letter = |c: char| c.is_alphanumeric() || c == '_';
        if letter(c) {
            word.push(c);
        } else if c == '\''
            && !word.is_empty()
            && !apostrophe
            && chars.peek().is_some_and(|c| letter(*c))
        {
            word.push(c);
            apostrophe = true;
        } else if !word.is_empty() {
            result.push(std::mem::take(&mut word));
            apostrophe = false;
        }
    }
    if !word.is_empty() {
        result.push(word);
    }
    if result == ["mm", "hmm"] || result == ["mmhmm"] {
        return vec!["mhm".into()];
    }
    result
}

struct Take {
    meta: ClipMeta,
    audio: speak::Audio,
    object: String,
}

fn validated_audio(
    root: &Path,
    index: &Connection,
    packs: &mut BTreeMap<String, Snapshot>,
    object: &str,
    disk: &DiskGuard,
) -> Result<speak::Audio> {
    let (rate, channels, frames, codec, pack, digest): (u32, u16, i64, String, String, String) =
        index.query_row(
            "SELECT rate,channels,frames,codec,pack,stored_sha256 FROM audio_objects WHERE id=?1",
            [object],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;
    ensure!(
        rate == 44100 && channels == 1 && frames > 0 && frames <= rate as i64 * 120,
        "invalid Jarvis audio geometry"
    );
    ensure!(
        codec == "f32-zstd-planar-v1",
        "bank audio is not native lossless"
    );
    if !packs.contains_key(&pack) {
        packs.insert(
            pack.clone(),
            Snapshot::open(&store::pack_path(root, &pack)?, disk)?,
        );
    }
    let db = &packs[&pack].db;
    let key = store::audio_key(object);
    let size: i64 = db.query_row("SELECT length(data) FROM blobs WHERE key=?1", [&key], |r| {
        r.get(0)
    })?;
    ensure!(
        (1..=32 * 1024 * 1024).contains(&size),
        "invalid encoded bank clip size"
    );
    let bytes: Vec<u8> =
        db.query_row("SELECT data FROM blobs WHERE key=?1", [&key], |r| r.get(0))?;
    ensure!(hash(&bytes) == digest, "stored audio hash mismatch");
    let pcm = audio::decode_object(rate, channels, frames as usize, &codec, &bytes)?;
    ensure!(pcm.samples.iter().all(|s| s.is_finite()), "nonfinite audio");
    ensure!(
        audio::native_id(rate, channels, &pcm.samples) == object,
        "PCM identity hash mismatch"
    );
    Ok(speak::Audio {
        samples: pcm.samples,
        sample_rate: rate,
        first_audio_ms: 0.,
        synthesis_ms: 0.,
    })
}

fn retained_asr(settings: &Value, text: &str) -> Result<()> {
    let asr = &settings["asr"];
    ensure!(
        asr["passed"] == true && asr["expected"].as_str() == Some(text),
        "missing retained ASR pass"
    );
    let recognized = asr["text"]
        .as_str()
        .context("ASR pass lacks recognized text")?;
    ensure!(
        words(recognized) == words(text),
        "retained ASR words mismatch"
    );
    Ok(())
}

pub fn run(from: &Path, into: &Path, apply: bool) -> Result<Report> {
    ensure_lab_path(from)?;
    let from = from
        .canonicalize()
        .context("--from must be an existing lab bank")?;
    let into = into
        .canonicalize()
        .context("--into must be an existing Heimdall data directory")?;
    let disk = DiskGuard { data: into.clone() };
    disk.check(&into, 0)?;
    let destination = into.join("archive");
    let resolved = if destination.exists() {
        destination.canonicalize()?
    } else {
        destination.clone()
    };
    ensure!(
        !from.starts_with(&resolved) && !resolved.starts_with(&from),
        "source and destination archives must be separate"
    );
    let line_set = LineSet::new(into.join("voice-bank-lines.json"));
    let (active_lines, warning) = line_set.current();
    let active: BTreeMap<_, _> = active_lines
        .iter()
        .map(|l| (l.id.clone(), l.text.clone()))
        .collect();
    let source = Snapshot::open(&from.join(store::INDEX), &disk)?;
    let count: i64 = source
        .db
        .query_row("SELECT count(*) FROM clips", [], |r| r.get(0))?;
    ensure!(
        count > 0 && count <= (crate::lines::MAX_LINES * crate::TAKES) as i64,
        "empty or oversized source bank"
    );
    let mut stmt = source.db.prepare("SELECT object_id,origin,line_id,text,language,voice,generator,settings,sample_rate,samples FROM clips ORDER BY id")?;
    let mut rows = stmt.query([])?;
    let mut packs = BTreeMap::new();
    let mut takes = Vec::new();
    let mut unique = BTreeSet::new();
    let mut lines = BTreeSet::new();
    let mut acks = BTreeSet::new();
    let mut pcm_bytes = 0usize;
    while let Some(row) = rows.next()? {
        let object: String = row.get(0)?;
        ensure!(
            row.get::<_, String>(1)? == "bank",
            "source contains a non-bank clip"
        );
        let line: String = row.get(2)?;
        let text: String = row.get(3)?;
        let language: String = row.get(4)?;
        // A reply-bank line (the app's active lines, English), else an ack from the compiled catalogue in its
        // own language with its exact text.
        let ack = if active.get(&line) == Some(&text) {
            ensure!(language == "en", "bank clip is not English: {line}");
            None
        } else {
            let ack = crate::acks::by_id(&line)
                .filter(|a| a.text == text)
                .with_context(|| format!("line ID/text does not match app's active lines or the ack set: {line}"))?;
            ensure!(language == ack.lang, "ack clip language {language} is not {}: {line}", ack.lang);
            Some(ack)
        };
        ensure!(
            row.get::<_, String>(5)? == speak::drama3::VOICE,
            "bank clip is not Jarvis: {line}"
        );
        ensure!(
            row.get::<_, String>(6)? == speak::drama3::GENERATOR,
            "bank generator mismatch: {line}"
        );
        let settings: Value = serde_json::from_str(&row.get::<_, String>(7)?)?;
        retained_asr(&settings, &text).with_context(|| format!("line {line}"))?;
        if let Some(ack) = ack {
            ensure!(
                settings["asr"]["language"].as_str() == Some(ack.lang.as_str()),
                "ack clip's ASR pass was not made in {}: {line}",
                ack.lang
            );
            // A variant's takes were said with its delivery, a plain line's without one.
            ensure!(
                settings["direction"].as_str() == ack.delivery.as_ref().map(|d| d.direction.as_str()),
                "ack clip's delivery {} is not the ack's: {line}",
                settings["direction"]
            );
            acks.insert(line.clone());
        }
        ensure!(
            unique.insert((object.clone(), line.clone())),
            "duplicate source clip: {line}"
        );
        let audio = validated_audio(&from, &source.db, &mut packs, &object, &disk)
            .with_context(|| format!("line {line}"))?;
        ensure!(
            row.get::<_, u32>(8)? == audio.sample_rate
                && row.get::<_, i64>(9)? == audio.samples.len() as i64,
            "clip/catalogue geometry mismatch: {line}"
        );
        pcm_bytes = pcm_bytes
            .checked_add(audio.samples.len() * 4)
            .context("PCM size overflow")?;
        ensure!(
            pcm_bytes <= MAX_PCM_BYTES,
            "bank exceeds 512 MiB decoded PCM limit"
        );
        lines.insert(line.clone());
        takes.push(Take {
            object,
            meta: ClipMeta {
                origin: Origin::Bank,
                line_id: Some(line),
                text,
                language,
                voice: speak::drama3::VOICE.into(),
                generator: speak::drama3::GENERATOR.into(),
                settings,
            },
            audio,
        });
    }
    // Every source clip and PCM is now validated and retained. Nothing below
    // rereads mutable source data after the first destination write.
    let existing = if destination.join(store::INDEX).exists() {
        Some(Snapshot::open(&destination.join(store::INDEX), &disk)?)
    } else {
        None
    };
    let mut present = 0;
    let mut destination_packs = BTreeMap::new();
    for take in &takes {
        if let Some(db) = &existing {
            let old: Option<(String,String)> = db.db.query_row("SELECT text,settings FROM clips WHERE object_id=?1 AND origin='bank' AND line_id=?2 AND voice=?3 AND generator=?4",
                params![take.object,take.meta.line_id,take.meta.voice,take.meta.generator], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((text, settings)) = old {
                ensure!(
                    text == take.meta.text,
                    "existing clip text conflicts with import"
                );
                retained_asr(&serde_json::from_str(&settings)?, &text)?;
                present += 1;
            }
            if audio::catalogued(&db.db, &take.object)? {
                let back = validated_audio(
                    &destination,
                    &db.db,
                    &mut destination_packs,
                    &take.object,
                    &disk,
                )?;
                ensure!(
                    back.samples.len() == take.audio.samples.len()
                        && back
                            .samples
                            .iter()
                            .zip(&take.audio.samples)
                            .all(|(a, b)| a.to_bits() == b.to_bits()),
                    "existing audio conflicts with import"
                );
            }
        }
    }
    let mut report = Report {
        dry_run: !apply,
        lines: lines.len(),
        acks: acks.len(),
        validated: takes.len(),
        already_present: present,
        would_insert: takes.len() - present,
        inserted: 0,
        concurrent_safe: true,
        lines_warning: warning,
    };
    if !apply {
        return Ok(report);
    }
    disk.check(&destination, insert_reserve(pcm_bytes as u64, takes.len())?)?;
    let _lifetime = compact::reader_lock(&destination)?;
    ensure!(
        *line_set.current().0 == *active_lines,
        "active voice-bank lines changed during validation; retry import"
    );
    disk.check(&destination, insert_reserve(pcm_bytes as u64, takes.len())?)?;
    let target = Store::open(&destination)?;
    let count = takes.len();
    let mut remaining = pcm_bytes as u64;
    for (index, take) in takes.into_iter().enumerate() {
        disk.check(&destination, insert_reserve(remaining, count - index)?)?;
        let stored = target.insert(&take.meta, &take.audio)?;
        remaining -= take.audio.samples.len() as u64 * 4;
        report.inserted += usize::from(stored.inserted);
        let back = target.audio(stored.clip_id)?;
        ensure!(
            back.sample_rate == take.audio.sample_rate
                && back.samples.len() == take.audio.samples.len()
                && back
                    .samples
                    .iter()
                    .zip(&take.audio.samples)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
            "import read-back mismatch"
        );
    }
    report.already_present = report.validated - report.inserted;
    report.would_insert = report.inserted;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn meta(line: &str, text: &str) -> ClipMeta {
        let mut settings = speak::drama3::settings();
        settings["asr"] = serde_json::json!({"passed":true,"expected":text,"text":text});
        ClipMeta {
            origin: Origin::Bank,
            line_id: Some(line.into()),
            text: text.into(),
            language: "en".into(),
            voice: speak::drama3::VOICE.into(),
            generator: speak::drama3::GENERATOR.into(),
            settings,
        }
    }
    fn audio(value: f32) -> speak::Audio {
        speak::Audio {
            samples: vec![value; 441],
            sample_rate: 44100,
            first_audio_ms: 0.,
            synthesis_ms: 0.,
        }
    }
    fn fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let data = temp.path().join("data");
        let bank = Store::open(&source)?;
        bank.insert(&meta("sure", "Sure."), &audio(0.1))?;
        bank.insert(&meta("okay", "Okay."), &audio(0.2))?;
        let old = Store::open(&data.join("archive"))?;
        old.insert(
            &ClipMeta {
                voice: "M1".into(),
                generator: crate::SUPERTONIC_ID.into(),
                settings: crate::supertonic_settings("cpu"),
                ..meta("sure", "Sure.")
            },
            &audio(0.3),
        )?;
        let db = store::open(&data.join("archive"))?;
        store::ensure_session(&db, "keep-this-session-byte-for-byte")?;
        drop((db, old, bank));
        store::close_packs();
        Ok((temp, source, data))
    }
    fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, at: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(at).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, result);
                } else {
                    result.insert(
                        path.strip_prefix(root).unwrap().into(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut result = BTreeMap::new();
        walk(root, root, &mut result);
        result
    }
    fn rows(root: &Path) -> Result<BTreeMap<(String, String), Vec<Vec<rusqlite::types::Value>>>> {
        let mut paths = vec![store::INDEX.into()];
        paths.extend(
            store::packs(root)?
                .into_iter()
                .map(|p| format!("packs/{p}.sqlite")),
        );
        let mut result = BTreeMap::new();
        for file in paths {
            let snapshot = Snapshot::open(
                &root.join(&file),
                &DiskGuard {
                    data: root.parent().unwrap().into(),
                },
            )?;
            let tables = snapshot
                .db
                .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for table in tables {
                let mut stmt = snapshot
                    .db
                    .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))?;
                let columns = stmt.column_count();
                let values = stmt
                    .query_map([], |r| {
                        (0..columns)
                            .map(|i| r.get(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                result.insert((file.clone(), table), values);
            }
        }
        Ok(result)
    }
    #[test]
    fn import_refuses_maximum_disk_floor_before_snapshot_or_archive_writes() -> Result<()> {
        let (_temp, source, data) = fixture()?;
        std::fs::write(
            data.join("settings.json"),
            serde_json::to_vec(&serde_json::json!({
                "free_space_floor_bytes":u64::MAX
            }))?,
        )?;
        let source_before = files(&source);
        let before = files(&data);
        for apply in [false, true] {
            let error = run(&source, &data, apply).unwrap_err();
            assert!(error.to_string().contains("free-space floor"), "{error:#}");
            assert_eq!(before, files(&data));
            assert_eq!(source_before, files(&source));
        }
        let disk = DiskGuard { data: data.clone() };
        let error = Snapshot::open(&source.join(store::INDEX), &disk)
            .err()
            .unwrap();
        assert!(error.to_string().contains("free-space floor"));
        assert_eq!(before, files(&data));
        assert_eq!(source_before, files(&source));
        Ok(())
    }
    #[test]
    fn disk_guard_reloads_settings_and_checks_snapshot_and_insert_budgets() -> Result<()> {
        let data = tempfile::tempdir()?;
        let disk = DiskGuard {
            data: data.path().into(),
        };
        assert_eq!(disk.floor()?, DEFAULT_FREE_SPACE_FLOOR_BYTES);
        std::fs::write(data.path().join("settings.json"), b"{}")?;
        assert_eq!(disk.floor()?, DEFAULT_FREE_SPACE_FLOOR_BYTES);
        std::fs::write(
            data.path().join("settings.json"),
            br#"{"free_space_floor_bytes":1073741824}"#,
        )?;
        assert_eq!(disk.floor()?, 1024 * 1024 * 1024);
        disk.check(data.path(), insert_reserve(441 * 4, 1)?)?;
        assert!(disk.snapshot(u64::MAX).is_err());
        assert!(insert_reserve(u64::MAX, 1).is_err());
        std::fs::write(
            data.path().join("settings.json"),
            serde_json::to_vec(&serde_json::json!({"free_space_floor_bytes":u64::MAX}))?,
        )?;
        // A guard created before this settings edit must refuse the next insertion.
        assert!(disk
            .check(data.path(), insert_reserve(441 * 4, 1)?)
            .is_err());
        Ok(())
    }
    #[test]
    fn dry_run_is_zero_write_apply_preserves_every_old_row_and_second_import_inserts_nothing(
    ) -> Result<()> {
        let (_temp, source, data) = fixture()?;
        let source_before = files(&source);
        let before = files(&data);
        let preview = run(&source, &data, false)?;
        assert_eq!(
            (
                preview.validated,
                preview.lines,
                preview.would_insert,
                preview.inserted
            ),
            (2, 2, 2, 0)
        );
        assert_eq!(before, files(&data));
        assert_eq!(source_before, files(&source));
        let old_rows = rows(&data.join("archive"))?;
        let live = Store::open(&data.join("archive"))?; // A live app-style connection remains open.
        assert!(live
            .takes(speak::drama3::VOICE, speak::drama3::GENERATOR)?
            .is_empty());
        let applied = run(&source, &data, true)?;
        assert_eq!(applied.inserted, 2);
        assert_eq!(
            live.takes(speak::drama3::VOICE, speak::drama3::GENERATOR)?
                .len(),
            2
        );
        let again = run(&source, &data, true)?;
        assert_eq!(
            (again.inserted, again.would_insert, again.already_present),
            (0, 0, 2)
        );
        let after_rows = rows(&data.join("archive"))?;
        for (key, old) in old_rows {
            for row in old {
                assert!(
                    after_rows[&key].contains(&row),
                    "existing row changed in {key:?}"
                );
            }
        }
        assert_eq!(source_before, files(&source));
        Ok(())
    }
    #[test]
    fn every_rejection_happens_before_any_destination_write_even_when_only_the_last_take_is_bad(
    ) -> Result<()> {
        for change in [
            "origin='agent'",
            "line_id='unknown'",
            "text='Wrong.'",
            "language='pl'",
            "voice='M1'",
            "generator='supertonic-3'",
            "settings='{}'",
            "settings=json_set(settings,'$.asr.passed',json('false'))",
            "settings=json_set(settings,'$.asr.text','No.')",
            "sample_rate=16000",
            "samples=1",
        ] {
            let (_temp, source, data) = fixture()?;
            let db = Connection::open(source.join(store::INDEX))?;
            db.execute(
                &format!("UPDATE clips SET {change} WHERE id=(SELECT max(id) FROM clips)"),
                [],
            )?;
            // Deliberately leave the source's committed WAL uncheckpointed.
            let before = files(&data);
            let source_before = files(&source);
            assert!(run(&source, &data, true).is_err(), "accepted {change}");
            assert_eq!(before, files(&data));
            assert_eq!(source_before, files(&source));
        }
        Ok(())
    }
    #[test]
    fn corrupt_audio_hash_and_pcm_identity_are_rejected_before_writes() -> Result<()> {
        for native_identity in [false, true] {
            let (_temp, source, data) = fixture()?;
            let db = Connection::open(source.join(store::INDEX))?;
            if native_identity {
                let (id, pack): (String, String) = db.query_row(
                    "SELECT id,pack FROM audio_objects ORDER BY id LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let fake = "0".repeat(64);
                let pack_db = Connection::open(store::pack_path(&source, &pack)?)?;
                pack_db.execute(
                    "UPDATE blobs SET key=?1 WHERE key=?2",
                    params![store::audio_key(&fake), store::audio_key(&id)],
                )?;
                db.execute(
                    "UPDATE audio_objects SET id=?1 WHERE id=?2",
                    params![fake, id],
                )?;
                db.execute(
                    "UPDATE clips SET object_id=?1 WHERE object_id=?2",
                    params![fake, id],
                )?;
            } else {
                db.execute("UPDATE audio_objects SET stored_sha256='bad' WHERE id=(SELECT object_id FROM clips ORDER BY id DESC LIMIT 1)",[])?;
            }
            let before = files(&data);
            assert!(run(&source, &data, true).is_err());
            assert_eq!(before, files(&data));
        }
        Ok(())
    }
    #[test]
    fn import_waits_for_a_live_archive_writer_without_losing_its_rows() -> Result<()> {
        let (_temp, source, data) = fixture()?;
        let writer = store::open(&data.join("archive"))?;
        writer.execute_batch("BEGIN IMMEDIATE; INSERT INTO sessions(name,started_wall_ns) VALUES('concurrent-capture',456);")?;
        let (source2, data2) = (source.clone(), data.clone());
        let worker = std::thread::spawn(move || run(&source2, &data2, true));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !worker.is_finished(),
            "import must not commit through another writer's transaction"
        );
        writer.execute_batch("COMMIT")?;
        assert_eq!(worker.join().unwrap()?.inserted, 2);
        assert_eq!(
            writer.query_row(
                "SELECT started_wall_ns FROM sessions WHERE name='concurrent-capture'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            456
        );
        let _lifetime = compact::reader_lock(&data.join("archive"))?;
        assert_eq!(compact::collect_now(&data.join("archive"))?, Some(0), "a reader's shared lifetime lock keeps collection from removing anything");
        Ok(())
    }
    /// A lab bank of ack takes as `fill --set acks` stores them: the ack's language on the clip and on its ASR pass.
    fn ack_meta(id: &str, text: &str, lang: &str) -> ClipMeta {
        let mut m = meta(id, text);
        m.language = lang.into();
        m.settings["asr"]["language"] = lang.into();
        m
    }
    fn ack_fixture() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let data = temp.path().join("data");
        std::fs::create_dir(&data)?;
        let bank = Store::open(&source)?;
        bank.insert(&meta("okay", "Okay."), &audio(0.4))?; // a reply-bank take alongside
        bank.insert(&ack_meta("en_on_it", "On it.", "en"), &audio(0.5))?;
        bank.insert(&ack_meta("en_checking", "Checking.", "en"), &audio(0.6))?;
        bank.insert(&ack_meta("en_yes_sir", "Yes, sir.", "en"), &audio(0.7))?;
        drop(bank);
        store::close_packs();
        Ok((temp, source, data))
    }
    #[test]
    fn ack_takes_import_in_their_own_language_next_to_reply_lines() -> Result<()> {
        let (_temp, source, data) = ack_fixture()?;
        let preview = run(&source, &data, false)?;
        assert_eq!((preview.validated, preview.lines, preview.acks, preview.would_insert), (4, 4, 3, 4));
        assert!(!data.join("archive").exists(), "dry run writes nothing");
        assert_eq!(run(&source, &data, true)?.inserted, 4);
        let live = Store::open(&data.join("archive"))?;
        let takes = live.takes(speak::drama3::VOICE, speak::drama3::GENERATOR)?;
        for (id, lang, text) in [("en_yes_sir", "en", "Yes, sir."), ("en_on_it", "en", "On it."), ("okay", "en", "Okay.")] {
            let clip = live.describe(takes[id][0])?;
            assert_eq!((clip["language"].as_str(), clip["text"].as_str()), (Some(lang), Some(text)), "{id}");
        }
        // The ack runtime finds the take by its id.
        let ack = crate::acks::by_id("en_checking").unwrap();
        assert_eq!(crate::acks::takes(ack, &takes).len(), 1);
        assert_eq!(run(&source, &data, true)?.inserted, 0);
        Ok(())
    }
    #[test]
    fn ack_takes_with_wrong_text_unknown_id_or_wrong_language_are_rejected_before_any_write() -> Result<()> {
        for change in [
            "text='Checking now.'",
            "text='Checking'",
            "line_id='en_unknown'",
            "line_id='en_on_it'",
            "language='pl'",
            "language='de'",
            "settings=json_set(settings,'$.asr.language','pl')",
            "settings=json_remove(settings,'$.asr.language')",
            "settings=json_set(settings,'$.asr.text','Checking now.')",
            // Polish acks are gone: last night's Polish take, exact in every field, is refused.
            "line_id='pl_sprawdzam',text='Sprawdzam.',language='pl',settings=json_set(json_set(settings,'$.asr.language','pl'),'$.asr.text','Sprawdzam.')",
            "settings=json_set(settings,'$.asr.passed',json('false'))",
            // A plain line's take said with a delivery (or a variant's without one) is not that ack's take.
            "settings=json_set(settings,'$.direction','crisp and urgent')",
            "voice='M1'",
            "generator='supertonic-3'",
        ] {
            let (_temp, source, data) = ack_fixture()?;
            let db = Connection::open(source.join(store::INDEX))?;
            db.execute(&format!("UPDATE clips SET {change} WHERE line_id='en_checking'"), [])?;
            assert!(run(&source, &data, true).is_err(), "accepted {change}");
            assert!(!data.join("archive").exists(), "{change}: nothing written");
        }
        // An English reply line is still English only.
        let (_temp, source, data) = ack_fixture()?;
        Connection::open(source.join(store::INDEX))?
            .execute("UPDATE clips SET language='pl' WHERE line_id='okay'", [])?;
        assert!(run(&source, &data, true).is_err());
        Ok(())
    }
    #[test]
    fn active_override_controls_validation_and_acknowledgement_spelling_matches_the_fill_gate(
    ) -> Result<()> {
        let (_temp, source, data) = fixture()?;
        std::fs::write(
            data.join("voice-bank-lines.json"),
            br#"[{"id":"sure","text":"Different."}]"#,
        )?;
        assert!(run(&source, &data, true).is_err());
        let mut value = meta("mhm", "Mhm.").settings;
        value["asr"]["text"] = "Mm-hmm.".into();
        retained_asr(&value, "Mhm.")?;
        for text in ["Hmm.", "Mm-hmm no.", "Mm-hmm mm-hmm."] {
            value["asr"]["text"] = text.into();
            assert!(retained_asr(&value, "Mhm.").is_err());
        }
        assert_ne!(words("I can't."), words("I can."));
        Ok(())
    }
}
