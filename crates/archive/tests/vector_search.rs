//! The in-memory vector copy (`vector_index`) ranks exactly like the old scan (`timeline::search_scan`), stays current
//! as embeddings are written, changed and removed, reloads from its side file, survives a damaged file, and a database
//! without a file falls back to the scan. No model runs: vectors are synthetic.
use anyhow::Result;
use heimdall_archive::{store, timeline::{self, Search, SearchStats, Tag}, vector_index};
use rusqlite::{params, Connection};
use serde_json::json;
use std::path::Path;

const DIMS: usize = 24;
const KINDS: [&str; 5] = ["text", "speech_audio", "other_audio", "moment", "screen"];
const WALL: i64 = 1_800_000_000_000_000_000;

/// Deterministic pseudo-random numbers (no extra dev-dependency).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2. - 1.
    }
    fn vector(&mut self) -> Vec<f32> { (0..DIMS).map(|_| self.next()).collect() }
}

fn put_vector(db: &Connection, values: &[f32]) -> Result<String> {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let id = heimdall_archive::hash(&bytes);
    db.execute("INSERT OR IGNORE INTO vectors VALUES(?1,?2,?3)", params![id, values.len() as i64, zstd::bulk::compress(&bytes, 3)?])?;
    Ok(id)
}
fn put_chunk(db: &Connection, id: &str, session: i64, kind: &str, wall: i64, text: &str, values: Option<&[f32]>) -> Result<()> {
    let hash = format!("h-{id}-{text}");
    db.execute("INSERT OR REPLACE INTO timeline_chunks VALUES(?1,?2,?3,'utterance','mic',?4,?5,?6,?7,?8,'{}',?9)",
        params![id, session, kind, wall - WALL, wall - WALL + 1_000, wall, wall + 1_000, text, hash])?;
    if let Some(values) = values {
        let vector = put_vector(db, values)?;
        db.execute("INSERT OR REPLACE INTO chunk_embeddings VALUES(?1,'m','1',?2,?3,?4,?5)",
            params![id, DIMS as i64, vector, hash, heimdall_archive::wall_ns() as i64])?;
    }
    Ok(())
}
/// An archive with `n` embedded chunks across kinds, sessions, times, texts and tags, plus a few without embeddings
/// and one whose embedding is stale (its content changed afterwards).
fn archive(n: usize) -> Result<(tempfile::TempDir, Connection, Lcg)> {
    let t = tempfile::tempdir()?;
    let db = store::open(t.path())?;
    let mut rng = Lcg(7);
    for i in 0..n {
        // Sound chunks are not used: their speech-coverage check needs capture rows (covered in timeline.rs).
        let kind = KINDS[[0, 1, 3, 4][i % 4]];
        let text = if kind == "screen" { String::new() } else { format!("words {} {}", i % 7, if i % 5 == 0 { "jarvis" } else { "" }) };
        let v = rng.vector();
        put_chunk(&db, &format!("c{i:04}"), 1 + (i % 3) as i64, kind, WALL + i as i64 * 1_000_000_000, &text, Some(&v))?;
        if i % 6 == 0 { db.execute("INSERT INTO chunk_tags VALUES(?1,'speaker','User')", [format!("c{i:04}")])?; }
    }
    // Duplicate vectors: equal scores must break ties by id exactly like the scan.
    let twin = rng.vector();
    put_chunk(&db, "twin-b", 1, "text", WALL, "twin", Some(&twin))?;
    put_chunk(&db, "twin-a", 1, "text", WALL, "twin", Some(&twin))?;
    put_chunk(&db, "plain", 1, "text", WALL, "no embedding", None)?;
    put_chunk(&db, "stale", 1, "text", WALL, "before", Some(&rng.vector()))?;
    db.execute("UPDATE timeline_chunks SET content_hash='changed' WHERE id='stale'", [])?;
    Ok((t, db, rng))
}
fn read_only(root: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(root.join(store::INDEX), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?)
}
fn query(values: &[f32], limit: usize) -> Search {
    Search { vector: Some(json!({"model_id":"m","model_version":"1","values":values})), limit, ..Default::default() }
}
/// Run the search both ways; the copy must be used and agree exactly (ids, order, similarities).
fn same_as_scan(db: &Connection, q: &Search) -> Result<SearchStats> {
    let (rows, stats) = timeline::search_with_stats(db, q)?;
    assert_eq!(stats.path, "memory", "{stats:?}");
    let v: timeline::VectorSearch = serde_json::from_value(q.vector.clone().unwrap())?;
    let scan = timeline::search_scan(db, q, Some(&v))?;
    let a: Vec<_> = rows.iter().map(|c| (c.id.clone(), c.similarity)).collect();
    let b: Vec<_> = scan.iter().map(|c| (c.id.clone(), c.similarity)).collect();
    assert_eq!(a, b, "memory and scan disagree for {q:?}");
    Ok(stats)
}

#[test]
fn memory_ranking_matches_the_scan_with_and_without_filters() -> Result<()> {
    let (t, _w, mut rng) = archive(900)?;
    let db = read_only(t.path())?;
    for limit in [1, 10, 100, 300] {
        for _ in 0..4 { same_as_scan(&db, &query(&rng.vector(), limit))?; }
    }
    let base = query(&rng.vector(), 50);
    for q in [
        Search { kinds: vec!["screen".into(), "moment".into()], ..base.clone() },
        Search { session: Some(2), ..base.clone() },
        Search { tags: vec![Tag { key: "speaker".into(), value: "User".into() }], ..base.clone() },
        Search { text: Some("JARVIS".into()), ..base.clone() },
        Search { start_wall_ns: Some((WALL + 100_000_000_000) as u64), end_wall_ns: Some((WALL + 400_000_000_000) as u64), ..base.clone() },
        Search { kinds: vec!["text".into()], session: Some(1), text: Some("words".into()), ..base.clone() },
    ] {
        same_as_scan(&db, &q)?;
    }
    // Ties: the twins share a vector and come back in id order.
    let twin: Vec<f32> = {
        let w = store::open(t.path())?;
        let id: String = w.query_row("SELECT vector_id FROM chunk_embeddings WHERE chunk_id='twin-a'", [], |r| r.get(0))?;
        heimdall_archive::records::vector_values(&w, &id)?
    };
    let rows = timeline::search(&db, &query(&twin, 2))?;
    assert_eq!(rows.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["twin-a", "twin-b"]);
    // Never a chunk without a current embedding.
    let all = timeline::search(&db, &query(&rng.vector(), 500))?;
    assert!(all.iter().all(|c| c.id != "plain" && c.id != "stale"));
    Ok(())
}

#[test]
fn the_copy_follows_new_changed_and_removed_embeddings() -> Result<()> {
    let (t, w, mut rng) = archive(300)?;
    let db = read_only(t.path())?;
    let probe = rng.vector();
    let first = same_as_scan(&db, &query(&probe, 20))?;
    assert!(first.prepared.as_deref().is_some_and(|p| p.starts_with("built")), "{first:?}");
    // A new chunk equal to the probe must come first at once; a re-embedded one moves; a removed one disappears.
    put_chunk(&w, "new", 3, "text", WALL, "fresh words", Some(&probe))?;
    put_chunk(&w, "c0001", 2, "speech_audio", WALL, "edited", Some(&probe.iter().map(|x| -x).collect::<Vec<_>>()))?;
    let top_before: String = timeline::search(&db, &query(&probe, 1))?[0].id.clone();
    assert_eq!(top_before, "new");
    let victim = timeline::search(&db, &query(&probe, 3))?[1].id.clone();
    w.execute("DELETE FROM chunk_embeddings WHERE chunk_id=?1", [&victim])?;
    let stats = same_as_scan(&db, &query(&probe, 20))?;
    assert!(stats.prepared.as_deref().is_some_and(|p| p.contains("1 changed")), "{stats:?}");
    assert!(timeline::search(&db, &query(&probe, 50))?.iter().all(|c| c.id != victim));
    // The re-embedded chunk now sits at the bottom for the probe.
    let worst = timeline::search(&db, &query(&probe.iter().map(|x| -x).collect::<Vec<_>>(), 1))?;
    assert_eq!(worst[0].id, "c0001");
    for _ in 0..5 { same_as_scan(&db, &query(&rng.vector(), 40))?; }
    Ok(())
}

#[test]
fn a_restart_loads_the_side_file_and_a_damaged_one_is_rebuilt() -> Result<()> {
    let (t, w, mut rng) = archive(200)?;
    let db = read_only(t.path())?;
    let probe = rng.vector();
    same_as_scan(&db, &query(&probe, 10))?;
    let files: Vec<_> = std::fs::read_dir(t.path().join("vector-cache"))?.collect::<std::io::Result<_>>()?;
    assert_eq!(files.len(), 1, "one side file per model");
    let file = files[0].path();
    // A new process: the copy comes from the file, then catches up with what changed after it was written.
    vector_index::reset_one(&db);
    put_chunk(&w, "after", 1, "text", WALL, "later words", Some(&probe))?;
    let stats = same_as_scan(&db, &query(&probe, 10))?;
    let prepared = stats.prepared.unwrap_or_default();
    assert!(prepared.starts_with("loaded") && prepared.contains("1 changed"), "{prepared}");
    assert_eq!(timeline::search(&db, &query(&probe, 1))?[0].id, "after");
    // Truncated file: rebuilt from the index, same answers.
    let bytes = std::fs::read(&file)?;
    std::fs::write(&file, &bytes[..bytes.len() / 2])?;
    vector_index::reset_one(&db);
    let stats = same_as_scan(&db, &query(&probe, 10))?;
    assert!(stats.prepared.as_deref().is_some_and(|p| p.starts_with("built")), "{stats:?}");
    Ok(())
}

#[test]
fn a_database_without_a_file_falls_back_to_the_scan_and_says_so() -> Result<()> {
    let (t, _w, mut rng) = archive(30)?;
    let mut db = Connection::open_in_memory()?;
    db.restore(rusqlite::MAIN_DB, t.path().join(store::INDEX), None::<fn(rusqlite::backup::Progress)>)?;
    let (rows, stats) = timeline::search_with_stats(&db, &query(&rng.vector(), 5))?;
    assert_eq!((stats.path, rows.len()), ("scan", 5));
    assert!(stats.fallback.is_some_and(|f| f.contains("no file path")));
    Ok(())
}

/// Review 13 Nov: more equal scores than the re-score pool. The pool's cut must keep the scan's id order, and a deeper
/// round must not repeat or skip a row.
#[test]
fn a_tie_group_larger_than_the_pool_keeps_the_scans_order() -> Result<()> {
    let t = tempfile::tempdir()?;
    let w = store::open(t.path())?;
    let mut rng = Lcg(11);
    let same = rng.vector();
    for i in 0..(timeline::RESCORE_POOL + 150) { put_chunk(&w, &format!("t{i:05}"), 1, "text", WALL, "same", Some(&same))?; }
    for i in 0..50 { put_chunk(&w, &format!("o{i:03}"), 1, "text", WALL, "other", Some(&rng.vector()))?; }
    let db = read_only(t.path())?;
    for limit in [10, 100, 500] {
        let stats = same_as_scan(&db, &query(&same, limit))?;
        assert!(stats.rescored <= timeline::RESCORE_MAX, "{stats:?}");
    }
    // Filtered so most of the first pool is rejected: deeper rounds, still no duplicates.
    let rows = timeline::search(&db, &Search { text: Some("other".into()), ..query(&same, 50) })?;
    let mut ids: Vec<_> = rows.iter().map(|c| c.id.clone()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 50);
    same_as_scan(&db, &Search { text: Some("other".into()), ..query(&same, 50) })?;
    Ok(())
}

