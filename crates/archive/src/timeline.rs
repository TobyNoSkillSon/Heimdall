//! Searchable, model-free timeline projections. These are semantic chunks, unrelated to HAC3
//! cold table chunks. All clocks are capture host ns plus explicit Unix wall ns. Source/time
//! audio references resolve through `View` after objects move or callback rows are packed.
use crate::{hash, store, Record};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

pub const MOMENT_NS: u64 = 45_000_000_000;
pub const SOUND_NS: u64 = 20_000_000_000;
#[path = "screen_timeline.rs"]
pub mod recording;

pub const SCREEN_MAX_NS: u64 = 45_000_000_000;
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS timeline_chunks(id TEXT PRIMARY KEY, session INTEGER NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('text','speech_audio','other_audio','moment','screen')), role TEXT NOT NULL, source TEXT NOT NULL, start_ns INTEGER NOT NULL, end_ns INTEGER NOT NULL CHECK(end_ns>=start_ns), start_wall_ns INTEGER NOT NULL, end_wall_ns INTEGER NOT NULL CHECK(end_wall_ns>=start_wall_ns), text TEXT NOT NULL, details TEXT NOT NULL, content_hash TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS timeline_ingestion(seq INTEGER PRIMARY KEY AUTOINCREMENT,chunk_id TEXT UNIQUE NOT NULL);
INSERT OR IGNORE INTO timeline_ingestion(chunk_id) SELECT id FROM timeline_chunks WHERE NOT EXISTS(SELECT 1 FROM timeline_ingestion) ORDER BY start_wall_ns,id;
CREATE TRIGGER IF NOT EXISTS timeline_ingested AFTER INSERT ON timeline_chunks BEGIN INSERT OR IGNORE INTO timeline_ingestion(chunk_id) VALUES(new.id); END;
CREATE INDEX IF NOT EXISTS timeline_time ON timeline_chunks(start_wall_ns,end_wall_ns);
CREATE INDEX IF NOT EXISTS timeline_source ON timeline_chunks(session,source,start_ns,end_ns);
CREATE INDEX IF NOT EXISTS timeline_kind ON timeline_chunks(kind,start_wall_ns);
CREATE TABLE IF NOT EXISTS chunk_tags(chunk_id TEXT NOT NULL, key TEXT NOT NULL CHECK(key IN ('speaker','source','app','language','command_kind','agent','listening','window_title')), value TEXT NOT NULL, PRIMARY KEY(chunk_id,key,value)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS tag_lookup ON chunk_tags(key,value,chunk_id);
CREATE TABLE IF NOT EXISTS chunk_links(chunk_id TEXT NOT NULL, relation TEXT NOT NULL, target TEXT NOT NULL, PRIMARY KEY(chunk_id,relation,target)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS chunk_link_target ON chunk_links(relation,target,chunk_id);
CREATE TABLE IF NOT EXISTS chunk_embeddings(chunk_id TEXT NOT NULL, model_id TEXT NOT NULL, model_version TEXT NOT NULL, dimensions INTEGER NOT NULL CHECK(dimensions>0), vector_id TEXT NOT NULL, content_hash TEXT NOT NULL, created_wall_ns INTEGER NOT NULL, PRIMARY KEY(chunk_id,model_id,model_version)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS chunk_embedding_errors(chunk_id TEXT NOT NULL,model_id TEXT NOT NULL,model_version TEXT NOT NULL,content_hash TEXT NOT NULL,retry_wall_ns INTEGER NOT NULL,error TEXT NOT NULL,PRIMARY KEY(chunk_id,model_id,model_version)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS cold_capture_spans(chunk_id INTEGER NOT NULL,source TEXT NOT NULL,start_ns INTEGER NOT NULL,end_ns INTEGER NOT NULL,PRIMARY KEY(chunk_id,source)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS cold_capture_source ON cold_capture_spans(source,start_ns,end_ns);
CREATE TABLE IF NOT EXISTS cold_capture_indexed(chunk_id INTEGER PRIMARY KEY);
CREATE TABLE IF NOT EXISTS timeline_speakers(session INTEGER NOT NULL,source TEXT NOT NULL,origin INTEGER NOT NULL,speaker INTEGER NOT NULL,name TEXT NOT NULL,PRIMARY KEY(session,source,origin,speaker)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS timeline_state(session INTEGER NOT NULL, host_ns INTEGER NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(session,host_ns,key)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS timeline_finals(session INTEGER NOT NULL, source TEXT NOT NULL, end_ns INTEGER NOT NULL, forced INTEGER NOT NULL, PRIMARY KEY(session,source,end_ns)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS timeline_audio_backfill(session INTEGER NOT NULL,source TEXT NOT NULL,bucket INTEGER NOT NULL,last_id INTEGER NOT NULL,PRIMARY KEY(session,source,bucket)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS timeline_backfill(session INTEGER PRIMARY KEY, evidence_id INTEGER NOT NULL DEFAULT 0, utterance_rowid INTEGER NOT NULL DEFAULT 0, block_id INTEGER NOT NULL DEFAULT 0);
";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tag {
    pub key: String,
    pub value: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub relation: String,
    pub target: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub session: i64,
    pub kind: String,
    pub role: String,
    pub source: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub start_wall_ns: u64,
    pub end_wall_ns: u64,
    /// Verbatim quoted speech, without synthetic summaries or transcript action notes.
    pub text: String,
    pub details: Value,
    pub tags: Vec<Tag>,
    pub links: Vec<Link>,
    pub content_hash: String,
    /// Query result only; captured details are never overwritten with a rank score.
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub similarity: Option<f64>,
}
impl Chunk {
    fn new(
        session: i64,
        kind: &str,
        role: &str,
        source: &str,
        key: &str,
        start: u64,
        end: u64,
        wall: u64,
    ) -> Self {
        Self {
            id: format!("{session}:{kind}:{role}:{key}"),
            session,
            kind: kind.into(),
            role: role.into(),
            source: source.into(),
            start_ns: start,
            end_ns: end,
            start_wall_ns: wall,
            end_wall_ns: wall.saturating_add(end.saturating_sub(start)),
            text: String::new(),
            details: json!({}),
            tags: vec![],
            links: vec![],
            content_hash: String::new(),
            similarity: None,
        }
    }
    fn tag(&mut self, key: &str, value: &str) {
        if !value.is_empty() && !self.tags.iter().any(|t| t.key == key && t.value == value) {
            self.tags.push(Tag {
                key: key.into(),
                value: value.into(),
            });
        }
    }
    fn link(&mut self, relation: &str, target: impl ToString) {
        let target = target.to_string();
        if !self
            .links
            .iter()
            .any(|l| l.relation == relation && l.target == target)
        {
            self.links.push(Link {
                relation: relation.into(),
                target,
            });
        }
    }
}
fn base_tags(db: &Connection, c: &mut Chunk) -> Result<()> {
    c.tag("source", &c.source.clone());
    let meta: Option<String> = db.prepare_cached("SELECT s.metadata_json FROM main.sources s JOIN main.segments g ON s.segment=g.id WHERE g.session=?1 AND s.id=?2 AND s.added_ns<=?3 ORDER BY s.added_ns DESC LIMIT 1")?
        .query_row(params![c.session,c.source,c.start_ns as i64], |r| r.get(0)).optional()?;
    if let Some(meta) = meta {
        let v: Value = serde_json::from_str(&meta)?;
        if let Some(bundle) = v
            .pointer("/kind/Process/owner/bundle_id")
            .and_then(Value::as_str)
            .or_else(|| v.pointer("/kind/Process/bundle_id").and_then(Value::as_str))
        {
            c.tag("app", bundle);
        }
    }
    for key in ["agent", "listening"] {
        let state:Option<String>=db.prepare_cached("SELECT value FROM main.timeline_state WHERE session=?1 AND key=?2 AND host_ns<=?3 ORDER BY host_ns DESC LIMIT 1")?
            .query_row(params![c.session,key,c.start_ns as i64],|r|r.get(0)).optional()?;
        c.tag(key, state.as_deref().unwrap_or("unknown"));
        let changes:Vec<String>=db.prepare_cached("SELECT DISTINCT value FROM main.timeline_state WHERE session=?1 AND key=?2 AND host_ns>?3 AND host_ns<?4")?.query_map(params![c.session,key,c.start_ns as i64,c.end_ns as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for value in changes {
            c.tag(key, &value);
        }
    }
    c.link(
        "source_span",
        json!({"session":c.session,"source":c.source,"start_ns":c.start_ns,"end_ns":c.end_ns}),
    );
    Ok(())
}
fn put(db: &Connection, c: &mut Chunk) -> Result<()> {
    ensure!(
        c.start_ns <= c.end_ns && c.start_wall_ns <= c.end_wall_ns,
        "invalid chunk interval"
    );
    c.tags
        .sort_by(|a, b| (&a.key, &a.value).cmp(&(&b.key, &b.value)));
    c.links
        .sort_by(|a, b| (&a.relation, &a.target).cmp(&(&b.relation, &b.target)));
    c.content_hash.clear();
    c.content_hash = hash(&serde_json::to_vec(&c)?);
    let old: Option<String> = db
        .prepare_cached("SELECT content_hash FROM main.timeline_chunks WHERE id=?1")?
        .query_row([&c.id], |r| r.get(0))
        .optional()?;
    if old.as_deref() == Some(&c.content_hash) {
        return Ok(());
    }
    for table in ["chunk_embeddings", "chunk_embedding_errors"] {
        db.execute(&format!("DELETE FROM main.{table} WHERE chunk_id=?1"), [&c.id])?;
    }
    db.prepare_cached("INSERT INTO main.timeline_chunks VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) ON CONFLICT(id) DO UPDATE SET start_ns=excluded.start_ns,end_ns=excluded.end_ns,start_wall_ns=excluded.start_wall_ns,end_wall_ns=excluded.end_wall_ns,text=excluded.text,details=excluded.details,content_hash=excluded.content_hash")?
        .execute(params![c.id,c.session,c.kind,c.role,c.source,c.start_ns as i64,c.end_ns as i64,c.start_wall_ns as i64,c.end_wall_ns as i64,c.text,c.details.to_string(),c.content_hash])?;
    db.prepare_cached("DELETE FROM main.chunk_tags WHERE chunk_id=?1")?
        .execute([&c.id])?;
    db.prepare_cached("DELETE FROM main.chunk_links WHERE chunk_id=?1")?
        .execute([&c.id])?;
    for t in &c.tags {
        db.prepare_cached("INSERT INTO main.chunk_tags VALUES(?1,?2,?3)")?
            .execute(params![c.id, t.key, t.value])?;
    }
    for l in &c.links {
        db.prepare_cached("INSERT INTO main.chunk_links VALUES(?1,?2,?3)")?
            .execute(params![c.id, l.relation, l.target])?;
    }
    Ok(())
}
fn remove(db: &Connection, id: &str) -> Result<()> {
    let removed = load(db, id).ok();
    let moments: Vec<String> = db
        .prepare_cached(
            "SELECT chunk_id FROM main.chunk_links WHERE relation='member' AND target=?1",
        )?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    db.execute(
        "DELETE FROM main.chunk_links WHERE relation='member' AND target=?1",
        [id],
    )?;
    for table in ["chunk_tags", "chunk_links", "chunk_embeddings", "chunk_embedding_errors"] {
        db.execute(&format!("DELETE FROM main.{table} WHERE chunk_id=?1"), [id])?;
    }
    db.execute("DELETE FROM main.timeline_chunks WHERE id=?1", [id])?;
    for id in moments {
        if let Ok(c) = load(db, &id) {
            refresh_moment(
                db,
                &id,
                c.session,
                c.start_wall_ns,
                c.start_ns as i128 - c.start_wall_ns as i128,
            )?;
        }
    }
    if let Some(c) = removed {
        if c.kind == "speech_audio" {
            refresh_sounds(db, c.session, &c.source, c.start_ns, c.end_ns)?;
        }
    }
    Ok(())
}
pub fn load(db: &Connection, id: &str) -> Result<Chunk> {
    let mut c=db.prepare_cached("SELECT id,session,kind,role,source,start_ns,end_ns,start_wall_ns,end_wall_ns,text,details,content_hash FROM main.timeline_chunks WHERE id=?1")?.query_row([id],|r|Ok(Chunk {
        id:r.get(0)?,session:r.get(1)?,kind:r.get(2)?,role:r.get(3)?,source:r.get(4)?,start_ns:r.get::<_,i64>(5)? as u64,end_ns:r.get::<_,i64>(6)? as u64,start_wall_ns:r.get::<_,i64>(7)? as u64,end_wall_ns:r.get::<_,i64>(8)? as u64,text:r.get(9)?,details:serde_json::from_str(&r.get::<_,String>(10)?).unwrap_or(Value::Null),content_hash:r.get(11)?,tags:vec![],links:vec![],similarity:None }))?;
    c.tags = db
        .prepare_cached(
            "SELECT key,value FROM main.chunk_tags WHERE chunk_id=?1 ORDER BY key,value",
        )?
        .query_map([id], |r| {
            Ok(Tag {
                key: r.get(0)?,
                value: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    c.links=db.prepare_cached("SELECT relation,target FROM main.chunk_links WHERE chunk_id=?1 ORDER BY relation,target")?.query_map([id],|r|Ok(Link{relation:r.get(0)?,target:r.get(1)?}))?.collect::<rusqlite::Result<_>>()?;
    Ok(c)
}
/// `load`, but a chunk that no longer exists (retracted by a speaker revision, a transcript
/// revision or an emptied moment) is `None` rather than an error.
pub fn find(db: &Connection, id: &str) -> Result<Option<Chunk>> {
    match load(db, id) {
        Ok(c) => Ok(Some(c)),
        Err(e) if matches!(e.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
        Err(e) => Err(e),
    }
}
/// Call inside the same transaction that changes the final transcript projection. Repeated
/// decision updates are no-ops; a late speaker label also updates turns and moments.
/// Remove derived search chunks when a transcript revision retires a run.
pub fn remove_utterance(db:&Connection,session:i64,id:&str)->Result<()> {
    for kind in ["text","speech_audio"] {for role in ["utterance","turn"] {
        let chunk=format!("{session}:{kind}:{role}:{id}");
        remove(db,&chunk)?;
        db.execute("DELETE FROM timeline_ingestion WHERE chunk_id=?1",[&chunk])?;
    }}
    Ok(())
}
pub fn utterance(db: &Connection, session: i64, id: &str) -> Result<()> {
    project_utterance(db, session, id)?;
    let next:Option<String>=db.query_row("SELECT b.id FROM main.utterances a JOIN main.utterances b ON b.session=a.session AND b.source_id=a.source_id AND b.start_ns>=a.end_ns AND b.id<>a.id WHERE a.session=?1 AND a.id=?2 ORDER BY b.start_ns,b.id LIMIT 1",params![session,id],|r|r.get(0)).optional()?;
    if let Some(next) = next {
        project_utterance(db, session, &next)?;
    }
    Ok(())
}
fn project_utterance(db: &Connection, session: i64, id: &str) -> Result<()> {
    let (source,speaker,start,end,wall,text,words):(String,String,i64,i64,i64,String,String)=db.query_row("SELECT coalesce(source_id,''),coalesce(speaker,''),coalesce(start_ns,0),coalesce(end_ns,0),coalesce(wall_ms,0),coalesce(text,''),coalesce(words,'[]') FROM main.utterances WHERE session=?1 AND id=?2",params![session,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
    let words: Value = serde_json::from_str(&words)?;
    // The app appends bracketed command notes to Observation.text; timed words own the quote.
    let quote = words
        .as_array()
        .filter(|a| !a.is_empty())
        .map(|a| {
            a.iter()
                .filter_map(|w| w["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or(text);
    let mut c = Chunk::new(
        session,
        "text",
        "utterance",
        &source,
        id,
        start.max(0) as u64,
        end.max(start).max(0) as u64,
        (wall.max(0) as u64)
            .saturating_mul(1_000_000)
            .saturating_sub((end - start).max(0) as u64),
    );
    // A final's wall_ms is arrival time. Prefer the capture anchor whenever one exists.
    let anchor:Option<(i64,i64)>=db.query_row("SELECT wall_unix_ns,host_origin_ns FROM main.segments WHERE session=?1 AND host_origin_ns<=?2 ORDER BY host_origin_ns DESC LIMIT 1",params![session,start],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((wall, host)) = anchor {
        c.start_wall_ns = (wall as u64).saturating_add(c.start_ns.saturating_sub(host as u64));
        c.end_wall_ns = c.start_wall_ns + c.end_ns - c.start_ns;
    }
    c.text = quote;
    c.link("utterance", id);
    let own_voice = speaker == HEIMDALL_SPEAKER;
    let speaker_tag = if let Some(n) = speaker
        .strip_prefix("Speaker ")
        .and_then(|n| n.parse::<i32>().ok())
    {
        format!("unknown speaker {n}")
    } else if speaker.is_empty() || speaker == "audio" || speaker == "Speaker ?" {
        "unknown speaker".into()
    } else {
        speaker
    };
    c.tag("speaker", &speaker_tag);
    // Language is supplied by the pipeline, never guessed by this index.
    let language: Option<String> = db
        .query_row(
            "SELECT value FROM main.chunk_tags WHERE chunk_id=?1 AND key='language'",
            [&c.id],
            |r| r.get(0),
        )
        .optional()?;
    c.tag("language", language.as_deref().unwrap_or("unknown"));
    base_tags(db, &mut c)?;
    let sentence = last_sentence(&c.text);
    if let Some(a) = words.as_array() {
        let mut bytes = 0;
        for w in a.iter().rev() {
            bytes += w["text"].as_str().map_or(0, str::len) + usize::from(bytes > 0);
            if bytes >= sentence.len() {
                c.details["last_sentence_start_ns"] =
                    json!(w["start_ns"].as_u64().unwrap_or(c.start_ns));
                break;
            }
        }
    }
    put(db, &mut c)?;
    moments(db, &c)?;
    let mut audio = c.clone();
    audio.id = format!("{session}:speech_audio:utterance:{id}");
    audio.kind = "speech_audio".into();
    audio.link("text_chunk", &c.id);
    put(db, &mut audio)?;
    let mut turn = c.clone();
    turn.id = format!("{session}:text:turn:{id}");
    turn.role = "turn".into();
    let forced: Option<bool> = db
        .query_row(
            "SELECT forced FROM main.timeline_finals WHERE session=?1 AND source=?2 AND end_ns=?3",
            params![session, source, end],
            |r| r.get(0),
        )
        .optional()?;
    turn.details = json!({"boundary":match forced {Some(false)=>"smart_turn",Some(true)=>"forced",None=>"legacy_final"},"overlap":"one_sentence"});
    let prev:Option<(String,String,i64,String)>=db.query_row("SELECT id,coalesce(text,''),end_ns,coalesce(speaker,'') FROM main.utterances WHERE session=?1 AND source_id=?2 AND end_ns<=?3 AND id<>?4 ORDER BY end_ns DESC,id DESC LIMIT 1",params![session,source,start,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    // A long silence ends the conversation: the turn starts fresh (bug note 22 Oct 23:57, where a turn joined an
    // utterance from 16 minutes before).
    // Heimdall's own voice on a microphone (its echo through speakers, archived only) never joins a turn of
    // someone else's words, nor theirs one of its own: the turn glued the echo's last sentence to the user's next one and
    // carried its speaker, so his words read as Heimdall's (bug note 2026-10-24T13-35-27).
    let prev = prev.filter(|(_, _, prior_end, prior_speaker)| (start - prior_end) as u64 <= TURN_OVERLAP_MAX_GAP_NS && (prior_speaker == HEIMDALL_SPEAKER) == own_voice);
    if let Some((prior, _, prior_end, _)) = prev {
        // Do not bridge capture discontinuities, source removals or a Listening Off boundary.
        let gap:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM main.timeline_state WHERE session=?1 AND key='boundary' AND host_ns BETWEEN ?2 AND ?3)",params![session,prior_end,start],|r|r.get(0))?;
        if !gap {
            let prior_id = format!("{session}:text:utterance:{prior}");
            if let Ok(previous) = load(db, &prior_id) {
                let sentence = last_sentence(&previous.text);
                if !sentence.is_empty() {
                    turn.text = format!("{sentence} {}", turn.text);
                    turn.link("overlap_chunk", &prior_id);
                    let overlap = previous.details["last_sentence_start_ns"]
                        .as_u64()
                        .unwrap_or(previous.start_ns);
                    turn.start_wall_ns = turn
                        .start_wall_ns
                        .saturating_sub(turn.start_ns.saturating_sub(overlap));
                    turn.start_ns = overlap;
                    turn.links.retain(|l| l.relation != "source_span");
                    turn.link("source_span",json!({"session":session,"source":source,"start_ns":overlap,"end_ns":turn.end_ns}));
                    for tag in previous.tags.iter().filter(|t| t.key == "speaker") {
                        turn.tag(&tag.key, &tag.value);
                    }
                }
            }
        }
    }
    put(db, &mut turn)?;
    let mut audio = turn.clone();
    audio.id = format!("{session}:speech_audio:turn:{id}");
    audio.kind = "speech_audio".into();
    audio.link("text_chunk", &turn.id);
    put(db, &mut audio)?;
    refresh_sounds(db, session, &source, c.start_ns, c.end_ns)?;
    Ok(())
}
/// The speaker of Heimdall's own voice on a microphone (the app's label for its echo and its wake sound).
pub const HEIMDALL_SPEAKER: &str = "Heimdall";
/// The longest silence a turn chunk reaches back across for the previous utterance's last sentence.
pub const TURN_OVERLAP_MAX_GAP_NS: u64 = 30_000_000_000;
fn last_sentence(text: &str) -> &str {
    let text = text.trim();
    let trimmed = text.trim_end_matches(['.', '!', '?', '。']);
    let at = trimmed
        .rfind(['.', '!', '?', '。'])
        .map_or(0, |i| i + trimmed[i..].chars().next().unwrap().len_utf8());
    text[at..].trim()
}
/// Exact language from Observation; separate from the quote. Unknown languages stay absent.
pub fn utterance_language(db: &Connection, session: i64, id: &str, language: &str) -> Result<()> {
    if !language.is_empty() {
        db.execute(
            "DELETE FROM main.chunk_tags WHERE chunk_id=?1 AND key='language'",
            [format!("{session}:text:utterance:{id}")],
        )?;
        db.execute(
            "INSERT OR IGNORE INTO main.chunk_tags VALUES(?1,'language',?2)",
            params![format!("{session}:text:utterance:{id}"), language],
        )?;
    }
    utterance(db, session, id)
}

/// Cheap journal hook: only relevant events project, in the evidence transaction.
pub(crate) fn record(db: &Connection, session: i64, evidence: i64, r: &Record) -> Result<()> {
    let p = &r.payload;
    match r.kind.as_str() {
        "settings_applied" | "timeline_settings" => {
            let s = &p["settings"];
            for (key, on) in [
                ("agent", s["dry_run"].as_bool().map(|x| !x)),
                ("listening", s["paused"].as_bool().map(|x| !x)),
            ] {
                if let Some(on) = on {
                    state(db, session, r.host_ns, key, if on { "on" } else { "off" })?;
                }
            }
        }
        "heimdall_on" | "heimdall_off" => {
            state(
                db,
                session,
                r.host_ns,
                "listening",
                if r.kind == "heimdall_on" { "on" } else { "off" },
            )?;
            state(db, session, r.host_ns, "boundary", "listening")?;
        }
        "gap" | "source_removed" => {
            state(db, session, r.host_ns, "boundary", &r.source)?;
        }
        "listen_event" if p["event"] == "final_utterance" => {
            if let Some(end) = p["end_ns"].as_u64() {
                db.execute(
                    "INSERT OR REPLACE INTO main.timeline_finals VALUES(?1,?2,?3,?4)",
                    params![
                        session,
                        r.source,
                        end as i64,
                        p["forced"].as_bool().unwrap_or(true)
                    ],
                )?;
            }
        }
        "agent_context_routing" => {
            if let Some(id) = p["observation"]["id"].as_str() {
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM main.utterances WHERE session=?1 AND id=?2)",
                    params![session, id],
                    |r| r.get(0),
                )?;
                if exists {
                    utterance_language(
                        db,
                        session,
                        id,
                        p["observation"]["language"].as_str().unwrap_or(""),
                    )?;
                }
            }
        }
        "command_decision" => {
            let start = p["phrase_start_ns"].as_u64().unwrap_or(r.host_ns);
            let end = p["decider_text"]["end_ns"]
                .as_u64()
                .unwrap_or(r.host_ns)
                .max(start);
            let mut c = Chunk::new(
                session,
                "text",
                "command",
                &r.source,
                &evidence.to_string(),
                start,
                end,
                r.wall_ns.saturating_sub(r.host_ns.saturating_sub(start)),
            );
            c.text = p["transcript"].as_str().unwrap_or("").into();
            c.details = p.clone();
            c.link("evidence", evidence);
            for id in &r.utterance_ids {
                c.link("utterance", id);
            }
            c.tag(
                "command_kind",
                p["candidate"]["phrase_guess"]
                    .as_str()
                    .or_else(|| p["intent"].as_str())
                    .unwrap_or("unknown"),
            );
            anchor_wall(db, &mut c)?;
            base_tags(db, &mut c)?;
            put(db, &mut c)?;
            moments(db, &c)?;
        }
        "speaker_name" => {
            if let Some(label) = p.get("LabelChanged") {
                speaker_name(db, session, label)?;
            } else if let Some(coded) = p.get("VoiceCoded") {
                // A coded voice's label names its call speaker like a name (quiet speaker naming, 23 Oct).
                speaker_name(db, session, &json!({"track": coded["track"], "name": coded["code"]}))?;
            }
        }
        "speaker_segments_delta" => {
            speaker_delta(db, session, evidence, r)?;
        }
        // The recording branch calls `screen_frame` once its committed frame/segment IDs exist.
        _ => (),
    }
    Ok(())
}
fn state(db: &Connection, session: i64, host: u64, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO main.timeline_state VALUES(?1,?2,?3,?4)",
        params![session, host as i64, key, value],
    )?;
    Ok(())
}
fn speaker_delta(db: &Connection, session: i64, evidence: i64, r: &Record) -> Result<()> {
    let origin = r.payload["origin_ns"].as_u64().unwrap_or(r.host_ns);
    if let Some(rows) = r.payload["segments"].as_array() {
        for row in rows {
            let (Some(i), Some(a), Some(b), Some(speaker)) = (
                row["index"].as_u64(),
                row["segment"]["start_time"].as_f64(),
                row["segment"]["end_time"].as_f64(),
                row["segment"]["speaker"].as_i64(),
            ) else {
                continue;
            };
            if !a.is_finite() || !b.is_finite() || a < 0. || b <= a {
                continue;
            }
            let start = origin.saturating_add((a * 1e9) as u64);
            let end = origin.saturating_add((b * 1e9) as u64);
            let key = format!("{}:{origin}:{i}", r.source);
            let mut c = Chunk::new(
                session,
                "speech_audio",
                "speaker_turn",
                &r.source,
                &key,
                start,
                end,
                r.wall_ns.saturating_add(start.saturating_sub(r.host_ns)),
            );
            c.details = json!({"origin_ns":origin,"segment_index":i,"speaker":speaker});
            let name:Option<String>=db.query_row("SELECT name FROM main.timeline_speakers WHERE session=?1 AND source=?2 AND origin=?3 AND speaker=?4",params![session,r.source,origin as i64,speaker],|r|r.get(0)).optional()?;
            c.tag(
                "speaker",
                &name.unwrap_or_else(|| format!("unknown speaker {speaker}")),
            );
            c.link("evidence", evidence);
            anchor_wall(db, &mut c)?;
            base_tags(db, &mut c)?;
            put(db, &mut c)?;
            moments(db, &c)?;
            refresh_sounds(db, session, &r.source, start, end)?;
        }
    }
    if let Some(total) = r.payload["total_segments"].as_u64() {
        let ids:Vec<String>=db.prepare("SELECT id FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND role='speaker_turn' AND json_extract(details,'$.origin_ns')=?3 AND json_extract(details,'$.segment_index')>=?4")?.query_map(params![session,r.source,origin as i64,total as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            remove(db, &id)?;
        }
    }
    Ok(())
}

fn speaker_name(db: &Connection, session: i64, label: &Value) -> Result<()> {
    let (Some(source), Some(origin), Some(speaker), Some(name)) = (
        label["track"]["source"].as_str(),
        label["track"]["session"].as_u64(),
        label["track"]["speaker"].as_i64(),
        label["name"].as_str(),
    ) else {
        return Ok(());
    };
    db.execute(
        "INSERT OR REPLACE INTO main.timeline_speakers VALUES(?1,?2,?3,?4,?5)",
        params![session, source, origin as i64, speaker, name],
    )?;
    let ids:Vec<String>=db.prepare_cached("SELECT id FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND role='speaker_turn' AND json_extract(details,'$.origin_ns')=?3 AND json_extract(details,'$.speaker')=?4")?.query_map(params![session,source,origin as i64,speaker],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        let mut c = load(db, &id)?;
        c.tags.retain(|t| t.key != "speaker");
        c.tag("speaker", name);
        put(db, &mut c)?;
        moments(db, &c)?;
    }
    Ok(())
}
/// A voice renamed everywhere: every chunk tagged speaker `from` (its lines and call turns, all sessions) is tagged
/// `to`. Inside a transaction.
pub fn rename_speaker_tag(db: &Connection, from: &str, to: &str) -> Result<()> {
    let ids: Vec<String> = db.prepare("SELECT chunk_id FROM main.chunk_tags WHERE key='speaker' AND value=?1")?.query_map(params![from], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        let mut c = load(db, &id)?;
        // A moment takes its tags from its members: each member's `moments` refreshes it (re-putting a moment would
        // link it to itself).
        if c.kind == "moment" {
            continue;
        }
        c.tags.retain(|t| !(t.key == "speaker" && t.value == from));
        c.tag("speaker", to);
        put(db, &mut c)?;
        // Only a chunk that is already a moment's member refreshes it: turns and audio chunks never join one.
        let member: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM main.chunk_links WHERE relation='member' AND target=?1)", [&c.id], |r| r.get(0))?;
        if member {
            moments(db, &c)?;
        }
    }
    Ok(())
}
fn anchor_wall(db: &Connection, c: &mut Chunk) -> Result<()> {
    let anchor:Option<(i64,i64)>=db.query_row("SELECT wall_unix_ns,host_origin_ns FROM main.segments WHERE session=?1 AND host_origin_ns<=?2 ORDER BY host_origin_ns DESC LIMIT 1",params![c.session,c.start_ns as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((wall, host)) = anchor {
        c.start_wall_ns = (wall as u64).saturating_add(c.start_ns.saturating_sub(host as u64));
        c.end_wall_ns = c.start_wall_ns + c.end_ns - c.start_ns;
    }
    Ok(())
}

fn moments(db: &Connection, member: &Chunk) -> Result<()> {
    let previous: Vec<String> = db
        .prepare_cached(
            "SELECT chunk_id FROM main.chunk_links WHERE relation='member' AND target=?1",
        )?
        .query_map([&member.id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    db.execute(
        "DELETE FROM main.chunk_links WHERE relation='member' AND target=?1",
        [&member.id],
    )?;
    let first = member.start_wall_ns / MOMENT_NS;
    let last = member
        .end_wall_ns
        .saturating_sub(u64::from(member.end_wall_ns > member.start_wall_ns))
        / MOMENT_NS;
    for bucket in first..=last {
        let wall = bucket * MOMENT_NS;
        let id = format!("{}:moment:window:{bucket}", member.session);
        db.execute(
            "INSERT OR IGNORE INTO main.chunk_links VALUES(?1,'member',?2)",
            params![id, member.id],
        )?;
        refresh_moment(
            db,
            &id,
            member.session,
            wall,
            member.start_ns as i128 - member.start_wall_ns as i128,
        )?;
    }
    for id in previous {
        if let Ok(c) = load(db, &id) {
            refresh_moment(
                db,
                &id,
                c.session,
                c.start_wall_ns,
                c.start_ns as i128 - c.start_wall_ns as i128,
            )?;
        }
    }
    Ok(())
}
fn refresh_moment(db: &Connection, id: &str, session: i64, wall: u64, offset: i128) -> Result<()> {
    let members:Vec<String>=db.prepare_cached("SELECT t.id FROM main.timeline_chunks t JOIN main.chunk_links l ON l.target=t.id WHERE l.chunk_id=?1 AND l.relation='member' ORDER BY t.start_ns,t.id")?.query_map([id],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    if members.is_empty() {
        return remove(db, id);
    }
    let start = (wall as i128 + offset).max(0) as u64;
    let end = ((wall + MOMENT_NS) as i128 + offset).max(0) as u64;
    let mut c = Chunk::new(
        session,
        "moment",
        "window",
        "timeline",
        &format!("{}", wall / MOMENT_NS),
        start,
        end,
        wall,
    );
    c.end_wall_ns = wall + MOMENT_NS;
    let mut context = vec![];
    for id in members {
        let child = load(db, &id)?;
        c.link("member", &id);
        for t in &child.tags {
            c.tag(&t.key, &t.value);
        }
        if !child.text.is_empty() {
            if !c.text.is_empty() {
                c.text.push('\n');
            }
            c.text.push_str(&child.text);
        }
        context.push(json!({"chunk_id":child.id,"source":child.source,"tags":child.tags,"window_title":child.details["window_title"]}));
    }
    c.details = json!({"context":context});
    put(db, &mut c)
}

/// One nonzero app-audio span, supplied once per source/flush (not per callback). The
/// index excludes known speech at query/provider time, preserving the original span through
/// diarization revisions. Exact +0 silence produces no chunks. No VAD model is introduced.
pub fn audio_span(
    db: &Connection,
    session: i64,
    source: &str,
    start: u64,
    end: u64,
    wall: u64,
) -> Result<()> {
    if end <= start || source == "mic" || source.starts_with("mic:") {
        return Ok(());
    }
    for bucket in start / SOUND_NS..=(end - 1) / SOUND_NS {
        let a = start.max(bucket * SOUND_NS);
        let b = end.min((bucket + 1) * SOUND_NS);
        let key = format!("{source}:{bucket}");
        let mut c = Chunk::new(
            session,
            "other_audio",
            "sound",
            source,
            &key,
            a,
            b,
            wall.saturating_add(a - start),
        );
        if let Ok(old) = load(db, &c.id) {
            c.start_ns = c.start_ns.min(old.start_ns);
            c.end_ns = c.end_ns.max(old.end_ns);
            c.start_wall_ns = c.start_wall_ns.min(old.start_wall_ns);
            c.end_wall_ns = c.end_wall_ns.max(old.end_wall_ns);
        }
        c.details = json!({"classification":"sound_except_known_speech","silence":"exact_zero_omitted","sound_ranges_ns":sound_ranges(db,&c)?});
        base_tags(db, &mut c)?;
        put(db, &mut c)?;
    }
    Ok(())
}
/// The unresolved portions of an app sound chunk. Overlapping labelled speech is removed
/// even when it arrived after the sound chunk, so search/backfill never guesses a speaker.
pub fn sound_ranges(db: &Connection, c: &Chunk) -> Result<Vec<(u64, u64)>> {
    let spans:Vec<(i64,i64)>=db.prepare_cached("SELECT start_ns,end_ns FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND kind='speech_audio' AND role IN ('utterance','speaker_turn') AND start_ns<?4 AND end_ns>?3 ORDER BY start_ns")?
        .query_map(params![c.session,c.source,c.start_ns as i64,c.end_ns as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut ranges = vec![];
    let mut at = c.start_ns;
    for (a, b) in spans {
        let a = (a as u64).max(c.start_ns);
        if at < a {
            ranges.push((at, a.min(c.end_ns)));
        }
        at = at.max(b as u64);
    }
    if at < c.end_ns {
        ranges.push((at, c.end_ns));
    }
    Ok(ranges)
}

fn refresh_sounds(db: &Connection, session: i64, source: &str, start: u64, end: u64) -> Result<()> {
    let ids:Vec<String>=db.prepare_cached("SELECT id FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND kind='other_audio' AND start_ns<?4 AND end_ns>?3")?.query_map(params![session,source,start as i64,end as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for id in ids {
        let mut c = load(db, &id)?;
        c.details["sound_ranges_ns"] = json!(sound_ranges(db, &c)?);
        put(db, &mut c)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScreenChange {
    Unchanged,
    AppSwitch,
    PageLoad,
    Scroll,
    VisualChange,
}
/// Recording API hook. The caller owns meaningful-change detection and supplies committed
/// image/video references. Call on sampled frames (also unchanged) to enforce the 45 s cap;
/// close on stop/removal. This module never captures, polls windows or requests permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenFrame {
    pub display: String,
    pub host_ns: u64,
    pub wall_ns: u64,
    pub app: Option<String>,
    pub window_title: String,
    pub image_id: String,
    pub video_segment: Option<String>,
    pub change: ScreenChange,
}
pub fn screen_frame(db: &Connection, session: i64, frame: &ScreenFrame) -> Result<String> {
    transaction(db, || screen_frame_inner(db, session, frame))
}
fn screen_frame_inner(db: &Connection, session: i64, frame: &ScreenFrame) -> Result<String> {
    ensure!(
        !frame.display.is_empty() && !frame.image_id.is_empty(),
        "screen reference required"
    );
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.images WHERE id=?1)",
            [&frame.image_id],
            |r| r.get::<_, bool>(0)
        )?,
        "representative frame must be committed"
    );
    let prior:Option<String>=db.query_row("SELECT id FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND kind='screen' ORDER BY start_ns DESC LIMIT 1",params![session,frame.display],|r|r.get(0)).optional()?;
    if let Some(id) = prior {
        let mut old = load(db, &id)?;
        ensure!(
            frame.host_ns >= old.end_ns,
            "screen frames must be monotonic per display"
        );
        if old.details["closed"] != true {
            let boundary = frame.host_ns.min(old.start_ns + SCREEN_MAX_NS);
            old.end_ns = boundary;
            old.end_wall_ns = old.start_wall_ns + boundary - old.start_ns;
            put(db, &mut old)?;
            moments(db, &old)?;
            if matches!(frame.change, ScreenChange::Unchanged)
                && frame.host_ns < old.start_ns + SCREEN_MAX_NS
            {
                return Ok(id);
            }
        }
    }
    let mut c = Chunk::new(
        session,
        "screen",
        "state",
        &frame.display,
        &format!("{}:{}", frame.display, frame.host_ns),
        frame.host_ns,
        frame.host_ns,
        frame.wall_ns,
    );
    c.details = json!({"window_title":frame.window_title,"change":frame.change});
    c.link("image", &frame.image_id);
    if let Some(segment) = &frame.video_segment {
        c.link("video_segment", segment);
    }
    if let Some(app) = &frame.app {
        c.tag("app", app);
    }
    base_tags(db, &mut c)?;
    put(db, &mut c)?;
    moments(db, &c)?;
    Ok(c.id)
}
pub fn screen_stop(db: &Connection, session: i64, display: &str, host: u64) -> Result<()> {
    let id:Option<String>=db.query_row("SELECT id FROM main.timeline_chunks WHERE session=?1 AND source=?2 AND kind='screen' ORDER BY start_ns DESC LIMIT 1",params![session,display],|r|r.get(0)).optional()?;
    if let Some(id) = id {
        let mut c = load(db, &id)?;
        ensure!(host >= c.end_ns, "screen stop precedes last frame");
        c.end_ns = host.min(c.start_ns + SCREEN_MAX_NS);
        c.end_wall_ns = c.start_wall_ns + c.end_ns - c.start_ns;
        c.details["closed"] = json!(true);
        transaction(db, || {
            put(db, &mut c)?;
            moments(db, &c)
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    pub start_wall_ns: Option<u64>,
    pub end_wall_ns: Option<u64>,
    pub session: Option<i64>,
    pub kinds: Vec<String>,
    pub tags: Vec<Tag>,
    pub text: Option<String>,
    pub limit: usize,
    /// Exact cosine search: model_id, model_version and finite nonzero values.
    /// The app also accepts `query` here and embeds it with its resident provider.
    pub vector: Option<Value>,
}
impl Default for Search {
    fn default() -> Self {
        Self {
            start_wall_ns: None,
            end_wall_ns: None,
            session: None,
            kinds: vec![],
            tags: vec![],
            text: None,
            limit: 20,
            vector: None,
        }
    }
}
/// Read-only SQL API; all tag filters are exact and conjunctive. Vector requests
/// require a matching current model/version/dimension and never silently fall back.
pub fn search(db: &Connection, q: &Search) -> Result<Vec<Chunk>> {
    let vector = q.vector.as_ref().map(|v| serde_json::from_value::<VectorSearch>(v.clone()))
        .transpose().map_err(|e| anyhow::anyhow!("no embedding vector identity/values: {e}"))?;
    if let Some(v) = &vector {
        v.validate()?;
        ensure!(db.query_row("SELECT EXISTS(SELECT 1 FROM main.chunk_embeddings e JOIN main.timeline_chunks c ON c.id=e.chunk_id WHERE e.model_id=?1 AND e.model_version=?2 AND e.dimensions=?3 AND e.content_hash=c.content_hash)",params![v.model_id,v.model_version,v.values.len() as i64],|r|r.get::<_,bool>(0))?,"no embeddings available for this model/version/dimensions");
    }
    ensure!(
        (1..=100).contains(&q.limit)
            && q.kinds.len() <= 5
            && q.tags.len() <= 32
            && q.text.as_ref().is_none_or(|s| s.len() <= 8192),
        "invalid chunk search bounds"
    );
    ensure!(
        q.start_wall_ns
            .zip(q.end_wall_ns)
            .is_none_or(|(a, b)| a <= b),
        "reversed search interval"
    );
    let mut sql = String::from("SELECT c.id FROM main.timeline_chunks c WHERE 1=1");
    let mut args: Vec<rusqlite::types::Value> = vec![];
    let mut condition = |s: &str, value: rusqlite::types::Value| {
        sql.push_str(s);
        args.push(value);
    };
    if let Some(x) = q.start_wall_ns {
        ensure!(x <= i64::MAX as u64, "wall time exceeds sqlite range");
        condition(" AND c.end_wall_ns>=?", (x as i64).into());
    }
    if let Some(x) = q.end_wall_ns {
        ensure!(x <= i64::MAX as u64, "wall time exceeds sqlite range");
        condition(" AND c.start_wall_ns<=?", (x as i64).into());
    }
    if let Some(x) = q.session {
        condition(" AND c.session=?", x.into());
    }
    if let Some(x) = &q.text {
        condition(" AND instr(lower(c.text),lower(?))>0", x.clone().into());
    }
    if !q.kinds.is_empty() {
        sql.push_str(" AND c.kind IN (");
        sql.push_str(&vec!["?"; q.kinds.len()].join(","));
        sql.push(')');
        for kind in &q.kinds {
            ensure!(
                matches!(
                    kind.as_str(),
                    "text" | "speech_audio" | "other_audio" | "moment" | "screen"
                ),
                "unknown chunk kind"
            );
            args.push(kind.clone().into());
        }
    }
    for t in &q.tags {
        sql.push_str(" AND EXISTS(SELECT 1 FROM main.chunk_tags t WHERE t.chunk_id=c.id AND t.key=? AND t.value=?)");
        args.push(t.key.clone().into());
        args.push(t.value.clone().into());
    }
    // Sound chunks entirely covered by known speech are not other-audio hits.
    if let Some(v) = &vector {
        sql.push_str(" AND EXISTS(SELECT 1 FROM main.chunk_embeddings e WHERE e.chunk_id=c.id AND e.content_hash=c.content_hash AND e.model_id=? AND e.model_version=? AND e.dimensions=?)");
        args.extend([v.model_id.clone().into(),v.model_version.clone().into(),(v.values.len() as i64).into()]);
    }
    sql.push_str(" ORDER BY c.start_wall_ns DESC,c.id");
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
    let mut out = vec![];
    let mut ranked = vec![];
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let mut c = load(db, &id)?;
        if c.kind == "other_audio" {
            let ranges = sound_ranges(db, &c)?;
            if ranges.is_empty() {
                continue;
            }
            c.details["sound_ranges_ns"] = json!(ranges);
        }
        if let Some(v) = &vector {
            let id:String=db.query_row("SELECT vector_id FROM main.chunk_embeddings WHERE chunk_id=?1 AND model_id=?2 AND model_version=?3 AND content_hash=?4",params![c.id,v.model_id,v.model_version,c.content_hash],|r|r.get(0))?;
            let values=crate::records::vector_values(db,&id)?;
            ensure!(values.len()==v.values.len(),"stored embedding dimension mismatch");
            let score=cosine(&v.values,&values)?;
            // Keep only top-k: a scan does not retain all chunks/vectors in RAM.
            ranked.push((score,c));
            ranked.sort_by(|a,b|b.0.total_cmp(&a.0).then_with(||a.1.id.cmp(&b.1.id)));
            ranked.truncate(q.limit);
            continue;
        }
        out.push(c);
        if out.len() == q.limit {
            break;
        }
    }
    if vector.is_some() {
        for (score,mut chunk) in ranked {chunk.similarity=Some(score);out.push(chunk);}
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VectorSearch {
    pub model_id: String,
    pub model_version: String,
    pub values: Vec<f32>,
}
impl VectorSearch {
    fn validate(&self) -> Result<()> {
        ensure!(!self.model_id.is_empty() && !self.model_version.is_empty()
            && self.model_id.len()<=256 && self.model_version.len()<=256
            && (1..=32768).contains(&self.values.len()),"invalid vector search geometry/identity");
        cosine(&self.values,&self.values)?;
        Ok(())
    }
}
pub fn cosine(a:&[f32],b:&[f32]) -> Result<f64> {
    ensure!(a.len()==b.len() && !a.is_empty() && a.iter().chain(b).all(|v|v.is_finite()),"invalid cosine vectors");
    let dot:f64=a.iter().zip(b).map(|(a,b)|*a as f64 * *b as f64).sum();
    let norm=|v:&[f32]|v.iter().map(|v|(*v as f64).powi(2)).sum::<f64>().sqrt();
    let denominator=norm(a)*norm(b);
    ensure!(denominator>0.,"zero embedding vector");
    Ok((dot/denominator).clamp(-1.,1.))
}

// Native capture is buffered for one second independently of speaker evidence. Do not
// hand a still-uncommitted turn to the embedder, or embed a partial tail permanently.
// HAC3 catalogues give the same horizon without decoding historical PCM. Native
// callback intervals in each source/segment are disjoint and ordered: the nearest
// callback before the turn end proves overlap using the existing timeline index.
// Source removal also seals its tail, including sessions an old Quit left incomplete.
const SPEAKER_CAPTURE_REACHED: &str = "(EXISTS(SELECT 1 FROM main.segments s WHERE s.session=c.session AND (SELECT b.host_ns+b.frames*1000000000/b.rate FROM main.blocks b WHERE b.segment=s.id AND b.source_id=c.source ORDER BY b.host_ns DESC LIMIT 1)>=c.end_ns) OR EXISTS(SELECT 1 FROM main.cold_capture_spans r JOIN main.cold_chunks k ON k.id=r.chunk_id JOIN main.segments s ON s.id=k.scope WHERE s.session=c.session AND r.source=c.source AND r.end_ns>=c.end_ns) OR (SELECT source_state.removed_ns FROM main.sources source_state JOIN main.segments s ON s.id=source_state.segment WHERE s.session=c.session AND source_state.id=c.source ORDER BY s.id DESC LIMIT 1) IS NOT NULL OR EXISTS(SELECT 1 FROM main.sessions s WHERE s.id=c.session AND s.complete_wall_ns IS NOT NULL))";
const SPEAKER_CAPTURE_OVERLAPS: &str = "(EXISTS(SELECT 1 FROM main.segments s WHERE s.session=c.session AND (SELECT b.host_ns+b.frames*1000000000/b.rate FROM main.blocks b WHERE b.segment=s.id AND b.source_id=c.source AND b.host_ns<c.end_ns ORDER BY b.host_ns DESC LIMIT 1)>c.start_ns) OR EXISTS(SELECT 1 FROM main.cold_capture_spans r JOIN main.cold_chunks k ON k.id=r.chunk_id JOIN main.segments s ON s.id=k.scope WHERE s.session=c.session AND r.source=c.source AND r.start_ns<c.end_ns AND r.end_ns>c.start_ns))";

/// Repair old sample-availability failures even when the evidence checkpoint is done.
/// Retire impossible projections only after capture has passed them (or the session
/// completed); a live tail still waiting for its commit remains eligible later.
fn repair_speaker_samples(root: &Path, more: &mut impl FnMut() -> bool) -> Result<usize> {
    let db = store::open(root)?;
    // Historical revisions/removals left orphan error rows. Their old content
    // cannot be retried, so retire them in a bounded batch as well. Ask admission
    // only for actual work; an empty repair pass must not spend backfill's budget.
    let has_stale: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM main.chunk_embedding_errors e LEFT JOIN main.timeline_chunks c ON c.id=e.chunk_id WHERE c.id IS NULL OR c.content_hash<>e.content_hash)", [], |r| r.get(0))?;
    if has_stale {
        if !more() { return Ok(0); }
        return Ok(db.execute("DELETE FROM main.chunk_embedding_errors WHERE (chunk_id,model_id,model_version) IN (SELECT e.chunk_id,e.model_id,e.model_version FROM main.chunk_embedding_errors e LEFT JOIN main.timeline_chunks c ON c.id=e.chunk_id WHERE c.id IS NULL OR c.content_hash<>e.content_hash LIMIT 16)", [])?);
    }
    let sql = format!("SELECT c.id FROM main.timeline_chunks c WHERE c.role='speaker_turn' AND {SPEAKER_CAPTURE_REACHED} AND (NOT {SPEAKER_CAPTURE_OVERLAPS} OR EXISTS(SELECT 1 FROM main.chunk_embedding_errors e WHERE e.chunk_id=c.id AND (e.content_hash<>c.content_hash OR (e.error LIKE 'audio chunk % has no captured samples for %' AND e.retry_wall_ns<{RETIRED})))) ORDER BY c.id LIMIT 16");
    let ids: Vec<String> = db.prepare(&sql)?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut repaired = 0;
    for id in ids {
        if !more() { break; }
        let _reader = crate::compact::reader_lock(root)?;
        let c = load(&db, &id)?;
        let refs = audio_refs(root, &c)?;
        transaction(&db, || {
            if refs.is_empty() {
                remove(&db, &id)?;
            } else {
                // A retired row stays: the embedder already found every range it uses empty (review, 23 Oct:
                // deleting it here retried and re-retired the same turn on every pass).
                db.execute("DELETE FROM main.chunk_embedding_errors WHERE chunk_id=?1 AND (content_hash<>?2 OR (error LIKE 'audio chunk % has no captured samples for %' AND retry_wall_ns<?3))", params![id,c.content_hash,RETIRED])?;
            }
            Ok(())
        })?;
        repaired += 1;
    }
    Ok(repaired)
}

/// A bounded, resumable migration. Replays evidence with stable IDs (including cold HAC3),
/// then final rows and grouped nonzero capture spans. Checkpoints commit with projections.
/// `more` is checked between records/rows; a closed gate never advances its checkpoint.
/// Uses temp_store=FILE via View and a bounded decompressor, never loads archive audio.
/// Records the backfill replays per transaction.
const BATCH: usize = 32;
/// Live jobs waiting this long stop the backfill's replay (it resumes from its checkpoint on the next pass).
const LIVE_FIRST: std::time::Duration = std::time::Duration::from_millis(250);
pub fn backfill(root: &Path, more: &mut impl FnMut() -> bool) -> Result<usize> {
    // Old HAC3 capture containers predate source/time catalogues. Index their geometry
    // once, in bounded batches, so an embedding read never reconstructs a whole session.
    let indexed=crate::chunks::backfill_capture_spans(root,more)?;
    if indexed>0 {return Ok(indexed);}
    let repaired = repair_speaker_samples(root, more)?;
    if repaired > 0 { return Ok(repaired); }
    let mut count = 0;
    for name in store::sessions(root)? {
        let index = store::open(root)?;
        let session = store::find_session(&index, &name)?.unwrap();
        let done:bool=index.query_row("SELECT EXISTS(SELECT 1 FROM main.timeline_backfill b WHERE session=?1 AND evidence_id>=max(coalesce((SELECT max(id) FROM main.evidence WHERE session=?1),0),coalesce((SELECT max(last_id) FROM main.cold_chunks WHERE kind='evidence' AND scope=?1),0)) AND utterance_rowid>=coalesce((SELECT max(rowid) FROM main.utterances WHERE session=?1),0) AND block_id>=max(coalesce((SELECT max(b.id) FROM main.blocks b JOIN main.segments s ON b.segment=s.id WHERE s.session=?1),0),coalesce((SELECT max(c.last_id) FROM main.cold_chunks c JOIN main.segments s ON c.scope=s.id WHERE c.kind='blocks' AND s.session=?1),0)))",[session],|r|r.get(0))?;
        if done {
            continue;
        }
        if !more() {
            break;
        }
        let view = crate::View::session(&root.join(name))?;
        let db = &view.db;
        let session = view.session_id();
        db.execute(
            "INSERT OR IGNORE INTO main.timeline_backfill(session) VALUES(?1)",
            [session],
        )?;
        let checkpoint: i64 = db.query_row(
            "SELECT evidence_id FROM main.timeline_backfill WHERE session=?1",
            [session],
            |r| r.get(0),
        )?;
        let next:Option<i64>=db.query_row("SELECT min(n) FROM (SELECT min(id) n FROM main.evidence WHERE session=?1 AND id>?2 UNION ALL SELECT min(max(first_id,?2+1)) FROM main.cold_chunks WHERE kind='evidence' AND scope=?1 AND last_id>?2)",params![session,checkpoint],|r|r.get(0))?;
        let mut evidence_through = checkpoint;
        if let Some(next) = next {
            evidence_through = next.saturating_add(crate::chunks::CHUNK_ROWS - 1);
            view.evidence_window(checkpoint, evidence_through)?;
        }
        let mut open = true;
        let mut pending = 0;
        let mut last = checkpoint;
        // Write lock first: record() reads before it writes (see `transaction`). This idle-only catch-up writes on its
        // own connection (the View's temp tables live there): at raised QoS, `BATCH` records per transaction, and it
        // stops as soon as the store's writer has live jobs waiting.
        let _raised = crate::live::Raised::new();
        db.execute_batch("BEGIN IMMEDIATE")?;
        let replay = if next.is_none() {
            Ok(())
        } else {
            view.scan_record_kinds(checkpoint, ",settings_applied,timeline_settings,heimdall_on,heimdall_off,gap,source_removed,listen_event,agent_context_routing,command_decision,speaker_name,speaker_segments_delta,", |id, r| {
                // One batch per call always runs, so the catch-up advances even under continuous live writes.
                if !more() || (count >= BATCH && crate::live::backlog().is_some_and(|(_, at)| at.elapsed() >= LIVE_FIRST)) {
                    open = false;
                    return Ok(false);
                }
                record(db, session, id, &r)?;
                last = id;
                pending += 1;
                count += 1;
                if pending == BATCH {
                    db.execute(
                        "UPDATE main.timeline_backfill SET evidence_id=?2 WHERE session=?1",
                        params![session, last],
                    )?;
                    // Commit every `BATCH` records so the writer never waits behind a long batch.
                    db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
                    pending = 0;
                }
                Ok(true)
            })
        };
        if let Err(e) = replay {
            if !db.is_autocommit() {
                let _ = db.execute_batch("ROLLBACK");
            }
            return Err(e);
        }
        if open && evidence_through > checkpoint {
            // Never checkpoint past the snapshot's last real row at a partial tail:
            // later appends can occupy the unused remainder of this numeric window.
            let seen: Option<i64> =
                db.query_row("SELECT max(id) FROM temp.evidence", [], |r| r.get(0))?;
            last = last.max(seen.unwrap_or(evidence_through));
            count += usize::from(pending == 0);
        }
        db.execute(
            "UPDATE main.timeline_backfill SET evidence_id=?2 WHERE session=?1",
            params![session, last],
        )?;
        db.execute_batch("COMMIT")?;
        if !open {
            break;
        }
        // Finish evidence before transcript projection, so state/language/turn boundaries
        // are replayed in order. New evidence will be picked up on the next pass.
        if next.is_some() {
            return Ok(count);
        }
        let checkpoint: i64 = db.query_row(
            "SELECT utterance_rowid FROM main.timeline_backfill WHERE session=?1",
            [session],
            |r| r.get(0),
        )?;
        // Collected before writing: a still-stepping SELECT holds a read snapshot, and each
        // projection's write lock would then be an upgrade that fails at once under contention.
        let pending: Vec<(i64, String)> = db
            .prepare("SELECT rowid,id FROM main.utterances WHERE session=?1 AND rowid>?2 ORDER BY rowid")?
            .query_map(params![session, checkpoint], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (rowid, id) in pending {
            if !more() {
                open = false;
                break;
            }
            transaction(db, || {
                utterance(db, session, &id)?;
                db.execute(
                    "UPDATE main.timeline_backfill SET utterance_rowid=?2 WHERE session=?1",
                    params![session, rowid],
                )?;
                Ok(())
            })?;
            count += 1;
        }
        if !open {
            break;
        }
        let checkpoint: i64 = db.query_row(
            "SELECT block_id FROM main.timeline_backfill WHERE session=?1",
            [session],
            |r| r.get(0),
        )?;
        let next_block:Option<i64>=db.query_row("SELECT min(n) FROM (SELECT min(b.id) n FROM main.blocks b JOIN main.segments s ON b.segment=s.id WHERE s.session=?1 AND b.id>?2 UNION ALL SELECT min(max(c.first_id,?2+1)) FROM main.cold_chunks c JOIN main.segments s ON c.scope=s.id WHERE c.kind='blocks' AND s.session=?1 AND c.last_id>?2)",params![session,checkpoint],|r|r.get(0))?;
        let Some(next_block) = next_block else {
            continue;
        };
        let block_through = next_block.saturating_add(crate::chunks::CHUNK_ROWS - 1);
        view.capture_window(checkpoint, block_through)?;
        // Aggregate callback geometry before projecting: one insertion per 20 s source bucket.
        // Bounded by the CHUNK_ROWS window; collected before writing (see utterances above).
        let buckets: Vec<(String, i64, i64, i64, i64, i64)> = db.prepare("SELECT b.source_id,min(b.host_ns),max(b.host_ns+b.frames*1000000000/b.rate),min(b.wall_ns),max(b.id),b.host_ns/20000000000 FROM temp.blocks b LEFT JOIN main.timeline_audio_backfill a ON a.session=?2 AND a.source=b.source_id AND a.bucket=b.host_ns/20000000000 WHERE b.id>?1 AND b.codec<>'zero' GROUP BY b.source_id,b.host_ns/20000000000 HAVING max(b.id)>coalesce(max(a.last_id),0) ORDER BY max(b.id)")?
            .query_map(params![checkpoint, session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (source, start, end, wall, id, bucket) in buckets {
            if !more() {
                open = false;
                break;
            }
            transaction(db, || {
                audio_span(db, session, &source, start as u64, end as u64, wall as u64)?;
                db.execute(
                    "INSERT OR REPLACE INTO main.timeline_audio_backfill VALUES(?1,?2,?3,?4)",
                    params![session, source, bucket, id],
                )?;
                Ok(())
            })?;
            count += 1;
        }
        if !open {
            break;
        }
        let seen: Option<i64> =
            db.query_row("SELECT max(id) FROM temp.blocks", [], |r| r.get(0))?;
        db.execute(
            "UPDATE main.timeline_backfill SET block_id=?2 WHERE session=?1",
            params![session, seen.unwrap_or(block_through)],
        )?;
        count += 1;
        return Ok(count);
    }
    count += recording::backfill(root, more)?;
    Ok(count)
}
/// One atomic projection write. Outermost, it takes the write lock first (`BEGIN IMMEDIATE`):
/// a deferred transaction that reads before writing must upgrade a read snapshot, and SQLite
/// never calls the busy handler for that upgrade, so any concurrent writer (the evidence writer,
/// compaction, backfill) failed it at once with "database is locked" (19 Oct: dropped live
/// finals and "Listening interrupted"). Taking the lock up front waits under `store::BUSY`.
/// Nested inside a caller's transaction it is a savepoint of that transaction.
/// Never call it while a statement on `db` is still stepping: that statement holds a read
/// snapshot and the write lock would again be an upgrade.
pub fn transaction<T>(db: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    let outer = db.is_autocommit();
    let _raised = (outer && !crate::live::on_writer()).then(crate::live::Raised::new);
    db.execute_batch(if outer { "BEGIN IMMEDIATE" } else { "SAVEPOINT timeline_projection" })?;
    match body() {
        Ok(result) => {
            if let Err(e) = db.execute_batch(if outer { "COMMIT" } else { "RELEASE timeline_projection" }) {
                // A failed COMMIT leaves the transaction open; never leak it into the next write.
                if outer && !db.is_autocommit() {
                    let _ = db.execute_batch("ROLLBACK");
                }
                return Err(e.into());
            }
            Ok(result)
        }
        Err(e) => {
            let _ = db.execute_batch(if outer {
                "ROLLBACK"
            } else {
                "ROLLBACK TO timeline_projection; RELEASE timeline_projection"
            });
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub version: String,
    pub dimensions: usize,
}
/// The model supplies modality-aware input loading and inference. `Chunk.links` includes the
/// immutable quote, utterance/evidence IDs, source spans and image/video references. Heavy
/// audio decoding/inference is outside any write transaction. No provider is installed here.
pub trait Embedder {
    fn model(&self) -> Model;
    fn embed(&mut self, root: &Path, chunk: &Chunk) -> Result<Vec<f32>>;
    fn embed_while(&mut self,root:&Path,chunk:&Chunk,_more:&mut dyn FnMut()->bool)->Result<Vec<f32>> {
        self.embed(root,chunk)
    }
    /// How many same-kind chunks one provider call may take. 1 = no grouping.
    fn batch_size(&self,_kind:&str)->usize {1}
    /// One result per chunk, in order. A failed chunk never fails its neighbours.
    fn embed_many_while(&mut self,root:&Path,chunks:&[Chunk],more:&mut dyn FnMut()->bool)->Vec<Result<Vec<f32>>> {
        chunks.iter().map(|c|self.embed_while(root,c,more)).collect()
    }
}
/// Same-kind companions for a group are looked up among this many following rows.
const GROUP_WINDOW: usize = 64;
#[derive(Debug, Clone, Copy)]
pub struct BackfillGate {
    pub on_ac: bool,
    pub idle_seconds: f64,
}
impl BackfillGate {
    pub fn allowed(self) -> bool {
        self.on_ac && self.idle_seconds.is_finite() && self.idle_seconds >= 300.
    }
}
/// One bounded embedding job. Re-checks AC+idle before inference AND publication. Content
/// revisions invalidate prior embeddings and an in-flight stale result is never published.
pub fn embed_batch(
    root: &Path,
    provider: &mut impl Embedder,
    limit: usize,
    gate: &mut impl FnMut() -> BackfillGate,
) -> Result<usize> {
    embed_selected(root,provider,limit,None,&mut ||gate().allowed())
}
/// Live work includes chunks overlapping the last hour and needs no AC/idle admission.
/// `more` still gates both inference and publication (stop, free disk, cancellation).
pub fn embed_live(
    root:&Path, provider:&mut impl Embedder, limit:usize, now:u64,
    more:&mut impl FnMut()->bool,
) -> Result<usize> {
    embed_selected(root,provider,limit,Some(live_since(now)),more)
}
/// Start of the live window at `now`: chunks overlapping the last hour. Inside it, chunks
/// wait in `LiveBuffer` (`embedding::LIVE_BUFFER`) and embed together in one warm burst.
pub fn live_since(now:u64)->u64 {now.saturating_sub(3_600_000_000_000)}
/// Rows `embed_selected` would pick: ?1 model id, ?2 version, ?4 `since` (NULL = all),
/// ?5 dimensions, ?6 wall clock for retry backoff. ?3 is the caller's LIMIT.
fn pending_condition() -> String {
    let member_reached = SPEAKER_CAPTURE_REACHED.replace("c.", "member.");
    let member_overlaps = SPEAKER_CAPTURE_OVERLAPS.replace("c.", "member.");
    format!("(c.role<>'speaker_turn' OR ({SPEAKER_CAPTURE_REACHED} AND {SPEAKER_CAPTURE_OVERLAPS})) AND (c.kind<>'moment' OR NOT EXISTS(SELECT 1 FROM main.chunk_links l JOIN main.timeline_chunks member ON member.id=l.target WHERE l.chunk_id=c.id AND l.relation='member' AND member.role='speaker_turn' AND NOT ({member_reached} AND {member_overlaps}))) AND (?4 IS NULL OR c.end_wall_ns>=?4) AND (c.kind<>'other_audio' OR coalesce(json_array_length(json_extract(c.details,'$.sound_ranges_ns')),1)>0) AND NOT EXISTS(SELECT 1 FROM main.chunk_embeddings e WHERE e.chunk_id=c.id AND e.model_id=?1 AND e.model_version=?2 AND e.dimensions=?5 AND e.content_hash=c.content_hash) AND NOT EXISTS(SELECT 1 FROM main.chunk_embedding_errors e WHERE e.chunk_id=c.id AND e.model_id=?1 AND e.model_version=?2 AND e.content_hash=c.content_hash AND e.retry_wall_ns>?6 AND e.error NOT LIKE '%database is locked%' AND e.error NOT LIKE '%database table is locked%')")
}
/// `end_wall_ns` of the oldest chunk `embed_live` would select at `now`, if any.
pub fn oldest_pending_live(root:&Path,model:&Model,now:u64)->Result<Option<u64>> {
    if !store::exists(root) {return Ok(None);}
    let db=store::read_index(root)?;
    let sql=format!("SELECT min(c.end_wall_ns) FROM main.timeline_chunks c WHERE {} AND ?3 IS NOT NULL",pending_condition());
    let oldest:Option<i64>=db.query_row(&sql,params![model.id,model.version,0i64,live_since(now) as i64,model.dimensions as i64,crate::wall_ns() as i64],|r|r.get(0))?;
    Ok(oldest.map(|n|n as u64))
}
/// Decides each pass whether live work runs. Once the oldest pending chunk is `buffer`
/// old, every pass embeds (one group per call) until a pass embeds nothing; then the
/// next chunk waits for a full buffer again. `Duration::ZERO` embeds whenever pending.
#[derive(Debug)]
pub struct LiveBuffer {buffer:std::time::Duration,flushing:bool}
impl LiveBuffer {
    pub fn new(buffer:std::time::Duration)->Self {Self{buffer,flushing:false}}
    pub fn flushing(&self)->bool {self.flushing}
    pub fn due(&mut self,root:&Path,model:&Model,now:u64)->Result<bool> {
        if self.flushing {return Ok(true);}
        let wait=self.buffer.as_nanos().min(u64::MAX as u128) as u64;
        self.flushing=oldest_pending_live(root,model,now)?.is_some_and(|oldest|now.saturating_sub(oldest)>=wait);
        Ok(self.flushing)
    }
    /// Report what the live pass embedded; nothing embedded ends the flush.
    pub fn embedded(&mut self,n:usize) {if n==0 {self.flushing=false;}}
}
/// Recent chunks without a current embedding whose text literally contains any of
/// `words` (case-insensitive), newest first: semantic search cannot see them yet.
/// `until`, `kinds` and `tags` narrow it like the search that asked.
pub fn pending_literal(db:&Connection,model:&Model,since:u64,until:Option<u64>,words:&[String],kinds:&[String],tags:&[Tag],limit:usize)->Result<Vec<String>> {
    if words.is_empty() || limit==0 {return Ok(vec![]);}
    let mut sql="SELECT c.id FROM main.timeline_chunks c WHERE c.end_wall_ns>=? AND NOT EXISTS(SELECT 1 FROM main.chunk_embeddings e WHERE e.chunk_id=c.id AND e.model_id=? AND e.model_version=? AND e.dimensions=? AND e.content_hash=c.content_hash) AND (".to_owned();
    let mut args:Vec<rusqlite::types::Value>=vec![(since.min(i64::MAX as u64) as i64).into(),model.id.clone().into(),model.version.clone().into(),(model.dimensions as i64).into()];
    sql+=&vec!["instr(lower(c.text),lower(?))>0";words.len()].join(" OR ");
    args.extend(words.iter().map(|w|w.clone().into()));
    sql.push(')');
    if !kinds.is_empty() {
        sql+=" AND c.kind IN (";sql+=&vec!["?";kinds.len()].join(",");sql.push(')');
        args.extend(kinds.iter().map(|k|k.clone().into()));
    }
    if let Some(until)=until {sql+=" AND c.start_wall_ns<=?";args.push((until.min(i64::MAX as u64) as i64).into());}
    for t in tags {
        sql+=" AND EXISTS(SELECT 1 FROM main.chunk_tags t WHERE t.chunk_id=c.id AND t.key=? AND t.value=?)";
        args.extend([t.key.clone().into(),t.value.clone().into()]);
    }
    sql+=" ORDER BY c.start_wall_ns DESC,c.id LIMIT ?";args.push((limit as i64).into());
    Ok(db.prepare(&sql)?.query_map(rusqlite::params_from_iter(args),|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)
}
/// Literal words for `pending_literal`: words of four or more letters, or the whole
/// query when it has none.
pub fn literal_words(query:&str)->Vec<String> {
    let mut words:Vec<String>=query.split(|c:char|!c.is_alphanumeric()).filter(|w|w.chars().count()>=4).map(str::to_lowercase).collect();
    // SQLite's lower() folds ASCII only: also match capitalised and upper-case forms of
    // non-ASCII words (Łódź, ŚRODA).
    let extra:Vec<String>=words.iter().filter(|w|!w.is_ascii()).flat_map(|w| {
        let mut chars=w.chars();
        let capital=chars.next().map(|c|c.to_uppercase().collect::<String>()+chars.as_str()).unwrap_or_default();
        [capital,w.to_uppercase()]
    }).collect();
    words.extend(extra);
    words.sort();words.dedup();words.truncate(24);
    if words.is_empty() && !query.trim().is_empty() {words.push(query.trim().to_lowercase());}
    words
}
/// `limit` counts groups: one provider call each. A group is the first eligible row in
/// order plus up to `batch_size-1` rows of the same kind from the next GROUP_WINDOW rows.
fn embed_selected(
    root:&Path, provider:&mut impl Embedder,limit:usize,since:Option<u64>,
    more:&mut impl FnMut()->bool,
) -> Result<usize> {
    ensure!(limit > 0 && limit <= 100, "embedding batch limit");
    if !more() {
        return Ok(0);
    }
    let model = provider.model();
    ensure!(
        !model.id.is_empty()
            && !model.version.is_empty()
            && (1..=32768).contains(&model.dimensions),
        "invalid embedding model identity/geometry"
    );
    let db = store::open(root)?;
    // Results are written by the store's writer: one small transaction per chunk.
    let queue = crate::live::Queue::shared(root)?;
    let failed = |id: &str, hash: &str, retry: i64, message: String| -> Result<()> {
        let (id, model_id, version, hash) = (id.to_owned(), model.id.clone(), model.version.clone(), hash.to_owned());
        queue.write(move |w| {
            w.execute("INSERT OR REPLACE INTO main.chunk_embedding_errors VALUES(?1,?2,?3,?4,?5,?6)", params![id, model_id, version, hash, retry, message])?;
            Ok(())
        })?;
        Ok(())
    };
    let selection = format!("SELECT c.id,c.kind FROM main.timeline_chunks c WHERE {} ORDER BY c.start_wall_ns DESC,c.id LIMIT ?3", pending_condition());
    let rows:Vec<(String,String)>=db.prepare(&selection)?.query_map(params![model.id,model.version,(limit+GROUP_WINDOW) as i64,since.map(|n|n as i64),model.dimensions as i64,crate::wall_ns() as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut taken = vec![false; rows.len()];
    let (mut done, mut groups, mut head) = (0, 0, 0);
    'groups: while groups < limit {
        while head < rows.len() && taken[head] { head += 1; }
        if head >= rows.len() || !more() {
            break;
        }
        groups += 1;
        let kind = &rows[head].1;
        let size = provider.batch_size(kind).clamp(1, 32);
        let mut picks = vec![head];
        taken[head] = true;
        for j in head + 1..rows.len().min(head + 1 + GROUP_WINDOW) {
            if picks.len() >= size { break; }
            if !taken[j] && rows[j].1 == *kind { taken[j] = true; picks.push(j); }
        }
        let _reader = crate::compact::reader_lock(root)?;
        let mut chunks = vec![];
        for id in picks.iter().map(|&i| &rows[i].0) {
            // Selection and work are not one transaction: a concurrent revision may
            // have retracted the chunk. Nothing is left to index.
            let Some(chunk) = find(&db, id)? else { continue; };
            if chunk.kind == "other_audio" && sound_ranges(&db, &chunk)?.is_empty() {
                continue;
            }
            // A cold container's catalogue is an envelope, so confirm exact rows before
            // invoking the model. Backfill retires old empty turns through the same lookup.
            if chunk.role == "speaker_turn" && audio_refs(root, &chunk)?.is_empty() {
                let message = format!("audio chunk {} has no captured samples for {} [{},{})", chunk.id, chunk.source, chunk.start_ns, chunk.end_ns);
                failed(id, &chunk.content_hash, crate::wall_ns().saturating_add(300_000_000_000) as i64, message)?;
                continue;
            }
            chunks.push(chunk);
        }
        if chunks.is_empty() {
            continue;
        }
        let results = if chunks.len() == 1 {
            vec![provider.embed_while(root, &chunks[0], more)]
        } else {
            provider.embed_many_while(root, &chunks, more)
        };
        ensure!(results.len() == chunks.len(), "embedding provider returned {} results for {} chunks", results.len(), chunks.len());
        for (chunk, result) in chunks.iter().zip(results) {
            let id = &chunk.id;
            let values = match result {
                Ok(v)=>v,
                Err(error)=> {
                    if !more() {break 'groups;}
                    if crate::embedding::database_busy(&error) || crate::embedding::deferred(&error) { continue; }
                    // The chunk was retracted or revised during inference (e.g. a moment
                    // member vanished). The failure belonged to old content; the new
                    // revision, if any, is selected afresh. Never record or report it.
                    if find(&db,id)?.is_none_or(|c|c.content_hash!=chunk.content_hash) { continue; }
                    // Audio that was never captured cannot arrive once capture has passed the
                    // chunk: retire it rather than retry every five minutes forever. A new
                    // revision (put) clears the row and makes the chunk eligible again.
                    let retry=if crate::embedding::no_captured_audio(&error) && capture_passed(&db,id)? {RETIRED}
                        else {crate::wall_ns().saturating_add(300_000_000_000) as i64};
                    let message=format!("{error:#}").chars().take(1024).collect::<String>();
                    failed(id, &chunk.content_hash, retry, message)?;
                    eprintln!("chunk embedding delayed {id}: {error:#}");
                    continue;
                }
            };
            ensure!(
                values.len() == model.dimensions && values.iter().all(|v| v.is_finite()),
                "embedding result geometry/nonfinite values"
            );
            if !more() {
                break 'groups;
            }
            let bytes = crate::audio::pcm_bytes(&values);
            let vector = hash(&bytes);
            let data = zstd::bulk::compress(&bytes, 1)?;
            let (id, model_id, version, dimensions, hash) = (id.clone(), model.id.clone(), model.version.clone(), model.dimensions as i64, chunk.content_hash.clone());
            let stored = queue.call(move |db| {
                // Retracted or revised during inference: the vector describes old content.
                if find(db, &id)?.is_none_or(|c| c.content_hash != hash) {
                    return Ok(false);
                }
                db.execute("INSERT OR IGNORE INTO main.vectors VALUES(?1,?2,?3)", params![vector, dimensions, data])?;
                db.execute(
                    "INSERT OR REPLACE INTO main.chunk_embeddings VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![id, model_id, version, dimensions, vector, hash, crate::wall_ns() as i64],
                )?;
                db.execute("DELETE FROM main.chunk_embedding_errors WHERE chunk_id=?1 AND model_id=?2 AND model_version=?3", params![id, model_id, version])?;
                Ok(true)
            })?;
            done += usize::from(stored);
        }
    }
    Ok(done)
}
/// `retry_wall_ns` of a permanently failed chunk: never selected again and never health.
const RETIRED: i64 = i64::MAX;
/// Capture can no longer add samples to this chunk: the source's capture reached past it,
/// the source or session closed, or it ended longer ago than any capture flush delay.
fn capture_passed(db: &Connection, id: &str) -> Result<bool> {
    let horizon = crate::wall_ns().saturating_sub(15 * 60 * 1_000_000_000) as i64;
    let sql = format!("SELECT ({SPEAKER_CAPTURE_REACHED}) OR c.end_wall_ns<?2 FROM main.timeline_chunks c WHERE c.id=?1");
    Ok(db.query_row(&sql, params![id, horizon], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(true))
}
/// Current-content failures survive retry backoff and successful work on other chunks.
/// Stale revisions/model versions cannot keep index health red, and neither can audio
/// that was never captured (waiting for a flush, or retired): the user cannot act on either.
pub fn embedding_error_status(root:&Path,model:&Model)->Result<Option<String>> {
    let db=store::read_index(root)?;
    let message:Option<String>=db.query_row("SELECT min(e.error) FROM chunk_embedding_errors e JOIN timeline_chunks c ON c.id=e.chunk_id AND c.content_hash=e.content_hash WHERE e.model_id=?1 AND e.model_version=?2 AND e.retry_wall_ns<?3 AND e.error NOT LIKE '%database is locked%' AND e.error NOT LIKE '%database table is locked%' AND e.error NOT LIKE 'audio chunk % has no captured samples for %'",params![model.id,model.version,RETIRED],|r|r.get(0))?;
    Ok(message.map(|m|embedding_failure_status(&m)))
}
/// The menu line for an indexing failure: one short plain sentence, never a chunk id,
/// path or SQL text. `cause` is a stored error or a job error rendered with `{:#}`.
pub fn embedding_failure_status(cause:&str)->String {
    let c=cause.to_ascii_lowercase();
    let has=|words:&[&str]|words.iter().any(|w|c.contains(w));
    let why=if has(&["no space left","disk full","storage full","enospc"]) {"the disk is full"}
        else if has(&["runtime assets unavailable"]) {"the search model is not installed"}
        else if has(&["start offline embedding worker"]) {"the search model could not start"}
        else if has(&["embedding worker","timed out","embeddinggemma","identity mismatch","geometry","nonfinite","zero embedding"]) {"the search model is not responding correctly"}
        else if has(&["image","frame","video"]) {"some screenshots cannot be read"}
        else if has(&["audio","pcm","block","sample"]) {"some recordings cannot be read"}
        else if has(&["sqlite","database","sql","query returned"]) {"the archive database returned an error"}
        else {"some items cannot be read"};
    format!("Search indexing is failing because {why}.")
}
/// Resolve a chunk's audio against both hot and cold callback rows. Interval references
/// remain valid across compaction; object/offset/rate are the *current* stored representation.
pub fn audio_refs(root: &Path, chunk: &Chunk) -> Result<Vec<Value>> {
    let db = store::open(root)?;
    let name: String = db.query_row(
        "SELECT name FROM main.sessions WHERE id=?1",
        [chunk.session],
        |r| r.get(0),
    )?;
    let view = crate::View::session(&root.join(name))?;
    view.capture_span(&chunk.source,chunk.start_ns,chunk.end_ns)?;
    audio_refs_from_view(&view,chunk)
}
/// Already bounded/pinned session view, shared with the decoder for this request.
pub fn audio_refs_from_view(view:&crate::View,chunk:&Chunk) -> Result<Vec<Value>> {
    let ranges = if chunk.kind == "other_audio" {
        sound_ranges(&view.db, chunk)?
    } else {
        vec![(chunk.start_ns, chunk.end_ns)]
    };
    let mut out = vec![];
    for (start, end) in ranges {
        let mut stmt=view.db.prepare("SELECT id,segment,source_id,host_ns,rate,channels,frames,codec,object_id,offset_frames,gain,discontinuity FROM temp.blocks WHERE source_id=?1 AND host_ns<?3 AND host_ns+frames*1000000000/rate>?2 ORDER BY host_ns,id")?;
        for row in stmt.query_map(params![chunk.source,start as i64,end as i64],|r|Ok(json!({"block_id":r.get::<_,i64>(0)?,"segment":r.get::<_,i64>(1)?,"source":r.get::<_,String>(2)?,"host_ns":r.get::<_,i64>(3)?,"native_rate":r.get::<_,i64>(4)?,"native_channels":r.get::<_,i64>(5)?,"native_frames":r.get::<_,i64>(6)?,"codec":r.get::<_,String>(7)?,"object_id":r.get::<_,Option<String>>(8)?,"offset_frames":r.get::<_,i64>(9)?,"gain":r.get::<_,f64>(10)?,"discontinuity":r.get::<_,bool>(11)?,"clip_start_ns":start,"clip_end_ns":end})))? {out.push(row?);}
    }
    Ok(out)
}
