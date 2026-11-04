//! Timeline retrieval with bounded projections and opaque, query-bound continuations.
//!
//! Token diet ("this is really, really ugly… this needs to be a lot more token efficient"): a
//! search answers one short line per distinct moment, not one block per stored row. The turn, utterance, text and
//! audio rows of one moment merge into one hit (`Moment`), a 45-second moment window merges with the utterance it
//! contains, and consecutive screen states of one app merge. Each hit carries a short handle (`h3`) that `id` takes
//! back (`Pages::handle`), a short source label ("Mic · the user", "Discord · Morgan", "Screen · Ego Lite"), and the
//! sentences that match the query, not the first 384 bytes. Semantic hits stop at a similarity floor (`floor`), and
//! the slots merging frees go to the next distinct moments. `output: "full"` adds whole texts, ids and related ids.
use agent::{
    delivery::DeliveryPermit,
    mcp_contract::{utc, wall_ns, TEXT_BYTES},
};
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
#[cfg(test)]
use rusqlite::params;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
#[derive(Clone)]
struct Continuation {
    permit: DeliveryPermit,
    expires: Instant,
    query: Value,
    watermark: i64,
    after: Option<(i64, String)>,
    detail: Option<(Value, usize)>,
    ranked_offset: usize,
    /// The similarity floor of the first page (later pages keep it).
    floor: Option<f64>,
    /// The rows of the moments earlier pages showed: a later row of one of them is not a new hit.
    shown: Vec<Row>,
}
#[derive(Default)]
pub struct Pages {
    cursors: Mutex<HashMap<String, Continuation>>,
    /// Short hit handles: number → chunk id, and back. The newest `HANDLES` are kept.
    handles: Mutex<(BTreeMap<u64, String>, HashMap<String, u64>, u64)>,
    serial: std::sync::atomic::AtomicU64,
}
const HANDLES: usize = 8192;
impl Pages {
    fn put(&self, state: Continuation) -> Result<String> {
        let mut cursors = self.cursors.lock().unwrap();
        cursors.retain(|_, c| c.expires > Instant::now());
        ensure!(
            cursors.len() < 512,
            "BUSY: too many live timeline continuations; repeat later"
        );
        // Short and unguessable enough: a cursor is also bound to its caller's permit (`get`).
        let id = format!("c{}", self.serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1);
        ensure!(
            cursors
                .values()
                .filter_map(|s| s.detail.as_ref())
                .map(|(r, _)| r.to_string().len())
                .sum::<usize>()
                + state
                    .detail
                    .as_ref()
                    .map_or(0, |(r, _)| r.to_string().len())
                <= 16 * 1024 * 1024,
            "BUSY: timeline detail continuation budget reached; repeat later"
        );
        cursors.insert(id.clone(), state);
        Ok(id)
    }
    fn get(&self, id: &str, permit: &DeliveryPermit) -> Result<Continuation> {
        let cursors = self.cursors.lock().unwrap();
        let c = cursors
            .get(id)
            .context("CURSOR_EXPIRED: repeat the original query")?;
        ensure!(
            c.permit.same_authority(permit) && c.permit.check().is_ok(),
            "CURSOR_EXPIRED: cursor belongs to a retired caller; repeat the original query"
        );
        ensure!(
            c.expires > Instant::now(),
            "CURSOR_EXPIRED: repeat the original query"
        );
        Ok(c.clone())
    }
    /// The handle (`h12`) for a chunk id: the same handle every time the id comes back while it is kept.
    pub fn handle(&self, id: &str) -> String {
        let mut guard = self.handles.lock().unwrap();
        let (by_number, by_id, next) = &mut *guard;
        if let Some(n) = by_id.get(id) {
            return format!("h{n}");
        }
        *next += 1;
        let n = *next;
        by_number.insert(n, id.to_owned());
        by_id.insert(id.to_owned(), n);
        while by_number.len() > HANDLES {
            let (_, old) = by_number.pop_first().unwrap();
            by_id.remove(&old);
        }
        format!("h{n}")
    }
    /// A handle back to its chunk id; any other id passes unchanged.
    pub(crate) fn resolve(&self, id: &str) -> Result<String> {
        let Some(n) = id.strip_prefix('h').and_then(|n| n.parse::<u64>().ok()) else { return Ok(id.to_owned()) };
        let guard = self.handles.lock().unwrap();
        guard.0.get(&n).cloned().context("HANDLE_EXPIRED: repeat the search for fresh handles")
    }
}
fn project(db: &Connection, id: &str) -> Result<Value> {
    let (kind, start, end, source, text): (String, u64, u64, String, String) = db.query_row(
        "SELECT kind,start_wall_ns,end_wall_ns,source,text FROM timeline_chunks WHERE id=?",
        [id],
        |r| {
            Ok((
                r.get(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
                r.get(3)?,
                r.get(4)?,
            ))
        },
    )?;
    let tags = tags(db, id)?;
    let value = |key: &str| tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let related = related(db, id)?;
    Ok(
        json!({"id":id,"kind":kind,"start":utc(start),"end":utc(end),"source":source,"speaker":value("speaker"),"app":value("app"),"label":label(&source,&kind,value("app").as_deref(),&tags_all(&tags,"speaker")),"text":text,"related_ids":related}),
    )
}
fn tags(db: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    Ok(db
        .prepare_cached("SELECT key,value FROM chunk_tags WHERE chunk_id=? ORDER BY key,value")?
        .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
fn tags_all(tags: &[(String, String)], key: &str) -> Vec<String> {
    tags.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
}
fn related(db: &Connection, id: &str) -> Result<Vec<String>> {
    Ok(db.prepare_cached("SELECT target FROM chunk_links WHERE chunk_id=? AND target NOT LIKE '{%' AND target NOT LIKE '[%' ORDER BY relation,target LIMIT 16")?.query_map([id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
}
/// One stored row, as much as merging and the line need.
#[derive(Clone, Debug)]
struct Row {
    id: String,
    kind: String,
    start: u64,
    end: u64,
    source: String,
    speakers: Vec<String>,
    app: Option<String>,
    text: String,
    /// A moment window's members, as utterance keys (`utterance_key`).
    members: Vec<String>,
}
fn row(db: &Connection, id: &str) -> Result<Row> {
    let (kind, start, end, source, text): (String, i64, i64, String, String) = db.query_row(
        "SELECT kind,start_wall_ns,end_wall_ns,source,text FROM timeline_chunks WHERE id=?",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let tags = tags(db, id)?;
    let members = if kind == "moment" {
        db.prepare_cached("SELECT target FROM chunk_links WHERE chunk_id=? AND relation='member'")?
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .filter_map(|t| utterance_key(t).map(str::to_owned))
            .collect()
    } else {
        vec![]
    };
    Ok(Row {
        id: id.to_owned(),
        kind,
        start: start as u64,
        end: end as u64,
        source,
        speakers: tags_all(&tags, "speaker"),
        app: tags.iter().find(|(k, _)| k == "app").map(|(_, v)| v.clone()),
        text,
        members,
    })
}
/// `56:speech_audio:turn:session-…:241` → `session-…:241`: the words of one utterance or turn, whichever row
/// (text or audio) stores them.
fn utterance_key(id: &str) -> Option<&str> {
    let mut parts = id.splitn(4, ':');
    let (_, kind, role, rest) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (matches!(kind, "text" | "speech_audio") && matches!(role, "turn" | "utterance")).then_some(rest)
}
const SECOND: u64 = 1_000_000_000;
fn overlaps(a: &Row, b: &Row, slack: u64) -> bool {
    a.start <= b.end + slack && b.start <= a.end + slack
}
/// Whether two rows are the same moment: one utterance's text and audio rows, the turn around it, the moment window
/// that contains it, or consecutive screen states of one app.
fn same_moment(a: &Row, b: &Row) -> bool {
    let key = |r: &Row| utterance_key(&r.id).map(str::to_owned);
    match (a.kind == "moment", b.kind == "moment") {
        (true, true) => (a.app == b.app || a.app.is_none() || b.app.is_none()) && overlaps(a, b, 0),
        (true, false) => key(b).is_some_and(|k| a.members.contains(&k)),
        (false, true) => key(a).is_some_and(|k| b.members.contains(&k)),
        _ if a.kind == "screen" || b.kind == "screen" => {
            a.kind == b.kind && a.source == b.source && a.app == b.app && overlaps(a, b, 30 * SECOND)
        }
        _ => {
            (key(a).is_some() && key(a) == key(b))
                || (a.source == b.source
                    && overlaps(a, b, SECOND)
                    && (a.speakers.is_empty() || b.speakers.is_empty() || a.speakers == b.speakers))
        }
    }
}
/// The rows of one distinct moment, best match first.
struct Moment {
    rows: Vec<Row>,
    similarity: Option<f64>,
}
impl Moment {
    /// The row the line shows: words over a moment window over app sound over a screen state, then the best ranked
    /// (rows join in rank or time order), so a window's other voices never replace the line that matched.
    fn primary(&self) -> &Row {
        let rank = |k: &str| match k { "text" | "speech_audio" => 0, "moment" => 1, "other_audio" => 2, _ => 3 };
        self.rows.iter().enumerate().min_by_key(|(i, r)| (rank(&r.kind), *i)).unwrap().1
    }
}
/// A bundle id as the app's short name: `com.hnc.Discord` → "Discord", `com.citrolabs.ego.lite` → "Ego Lite".
pub(crate) fn app_name(bundle: &str) -> String {
    let known = [
        ("com.t3tools.t3code", "T3 Code"), ("com.apple.QuickTimePlayerX", "QuickTime"), ("com.brave.Browser", "Brave"),
        ("ch.protonvpn.mac", "Proton VPN"), ("ch.protonmail.desktop", "Proton Mail"), ("com.google.Chrome", "Chrome"),
        ("com.apple.Safari", "Safari"), ("com.microsoft.VSCode", "VS Code"), ("com.valvesoftware.steam.helper", "Steam"),
        ("com.apple.UserNotificationCenter", "Notifications"),
    ];
    if let Some((_, n)) = known.iter().find(|(b, _)| *b == bundle) {
        return (*n).to_owned();
    }
    let parts: Vec<&str> = bundle.split('.').collect();
    let noise = |p: &&str| matches!(p.to_ascii_lowercase().as_str(), "app" | "mac" | "macos" | "desktop" | "helper");
    let mut words: Vec<&str> = parts.iter().skip(2).filter(|p| !noise(p)).copied().collect();
    if words.is_empty() {
        words = parts.get(1).into_iter().copied().collect();
    }
    if words.is_empty() {
        return bundle.to_owned();
    }
    words.iter().map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default() }).collect::<Vec<_>>().join(" ")
}
/// The short source label: "Mic · the user", "Discord · Morgan", "Screen · Ego Lite", never a device string.
fn label(source: &str, kind: &str, app: Option<&str>, speakers: &[String]) -> String {
    let app = app.map(app_name);
    let place = if source == "mic" || source.starts_with("mic:") {
        Some("Mic".to_owned())
    } else if source.starts_with("display:") || kind == "screen" {
        Some(match &app { Some(a) => format!("Screen · {a}"), None => "Screen".into() })
    } else {
        app.clone().or_else(|| Some(if kind == "moment" { "Moment".into() } else { "App audio".into() }))
    };
    let mut who: Vec<&str> = speakers.iter().map(String::as_str).filter(|s| Some(*s) != app.as_deref()).collect();
    who.dedup();
    let who = match who.len() { 0 => None, 1 | 2 => Some(who.join(", ")), n => Some(format!("{} +{}", who[..2].join(", "), n - 2)) };
    [place, who].into_iter().flatten().collect::<Vec<_>>().join(" · ")
}
/// Spoken fillers the line leaves out (the whole text keeps them).
fn tidy(text: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for w in text.split_whitespace() {
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_lowercase();
        if matches!(bare.as_str(), "uh" | "um" | "uhm" | "erm" | "eh" | "hmm") { continue; }
        // "the the", "to like to like": a stammered repeat of the previous word.
        if out.last().is_some_and(|p| p.eq_ignore_ascii_case(w)) { continue; }
        out.push(w);
    }
    out.join(" ")
}
const STOP: &[&str] = &["the", "and", "for", "from", "with", "that", "this", "what", "about", "talking", "said", "say", "says", "when", "where", "who", "was", "were", "did", "does", "his", "her", "their", "our", "you", "him", "heimdall", "something", "anything", "thing", "things", "some", "any", "into", "over", "like"];
/// The query's content words, as four-letter stems so a misheard "Jarvez" still matches "Jarvis".
fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !STOP.contains(&w.as_str()))
        .map(|w| w.chars().take(4).collect())
        .collect();
    out.sort();
    out.dedup();
    out
}
fn matches(sentence: &str, terms: &[String]) -> usize {
    let words: Vec<String> = sentence.split(|c: char| !c.is_alphanumeric()).map(str::to_lowercase).collect();
    terms.iter().filter(|t| words.iter().any(|w| w.starts_with(t.as_str()))).count()
}
/// Characters of text one line shows.
const LINE_CHARS: usize = 150;
/// The part of `text` a line shows: the sentences that match the query (in their order, "…" between), else the
/// start; and how many characters of the tidied text it leaves out.
fn snippet(text: &str, query_terms: &[String], budget: usize) -> (String, usize) {
    let clean = tidy(&text.replace('\n', " "));
    let total = clean.chars().count();
    if total <= budget {
        return (clean, 0);
    }
    let mut sentences: Vec<&str> = vec![];
    let mut begin = 0;
    for (i, c) in clean.char_indices() {
        if matches!(c, '.' | '?' | '!') && clean[i + 1..].starts_with(' ') {
            sentences.push(clean[begin..=i].trim());
            begin = i + 1;
        }
    }
    if begin < clean.len() { sentences.push(clean[begin..].trim()); }
    let scored: Vec<(usize, usize)> = sentences.iter().enumerate().map(|(i, s)| (i, matches(s, query_terms))).filter(|(_, n)| *n > 0).collect();
    let mut chosen: Vec<usize> = vec![];
    let mut used = 0;
    let mut by_score = scored.clone();
    by_score.sort_by_key(|(i, n)| (std::cmp::Reverse(*n), *i));
    for (i, _) in by_score {
        let len = sentences[i].chars().count();
        if used + len <= budget || chosen.is_empty() {
            chosen.push(i);
            used += len + 2;
        }
    }
    chosen.sort();
    let parts: Vec<String> = chosen.iter().map(|&i| cut(sentences[i], query_terms, budget)).collect();
    let shown = if parts.is_empty() { cut(&clean, &[], budget) } else { parts.join(" … ") };
    let kept = shown.chars().filter(|c| *c != '…').count();
    (shown, total.saturating_sub(kept))
}
/// One sentence within `budget` characters: a window around its first matching word, cut at word boundaries.
fn cut(sentence: &str, query_terms: &[String], budget: usize) -> String {
    let chars: Vec<char> = sentence.chars().collect();
    if chars.len() <= budget {
        return sentence.to_owned();
    }
    let lower = sentence.to_lowercase();
    let first = query_terms.iter().filter_map(|t| lower.find(t.as_str())).min().map_or(0, |b| lower[..b].chars().count());
    let mut from = first.saturating_sub(budget / 3);
    let mut to = (from + budget).min(chars.len());
    from = to.saturating_sub(budget);
    // Move to word boundaries, never past each other; a run without spaces (a path, CJK text) is cut hard.
    let (hard_from, hard_to) = (from, to);
    while from > 0 && from < to && !chars[from - 1].is_whitespace() { from += 1; }
    if from >= to { from = hard_from; }
    while to < chars.len() && to > from && !chars[to].is_whitespace() { to -= 1; }
    if to <= from { to = hard_to; }
    let body: String = chars[from..to].iter().collect();
    format!("{}{}{}", if from > 0 { "…" } else { "" }, body.trim(), if to < chars.len() { "…" } else { "" })
}
/// Semantic hits below this similarity are left out, and so are those more than `FLOOR_SPREAD` below the best:
/// EmbeddingGemma's scores for unrelated speech sit in a narrow band, so a fixed floor alone keeps noise.
const FLOOR_MIN: f64 = 0.70;
const FLOOR_SPREAD: f64 = 0.05;
fn floor(best: f64, by_text: bool) -> f64 {
    if by_text { FLOOR_MIN.max(best - FLOOR_SPREAD) } else { best - FLOOR_SPREAD }
}
/// Characters of whole text per hit with `output: "full"` (the rest by its handle).
const FULL_CHARS: usize = 1200;
/// One moment as the page carries it.
fn hit(db: &Connection, pages: &Pages, m: &Moment, query_terms: &[String], full: bool) -> Result<Value> {
    let p = m.primary();
    // Who said the shown words; a moment window's other voices would only lengthen the label.
    let mut speakers: Vec<String> = p.speakers.clone();
    if speakers.is_empty() { for r in &m.rows { for s in &r.speakers { if !speakers.contains(s) { speakers.push(s.clone()); } } } }
    let app = p.app.clone().or_else(|| m.rows.iter().find_map(|r| r.app.clone()));
    let mut out = json!({"h":pages.handle(&p.id),"start":utc(p.start),"end":utc(m.rows.iter().map(|r| r.end).max().unwrap_or(p.end)),"label":label(&p.source,&p.kind,app.as_deref(),&speakers)});
    if let Some(s) = m.similarity { out["similarity"] = json!(s); }
    if p.text.trim().is_empty() {
        if let Some(t) = m.rows.iter().find_map(|r| tags(db, &r.id).ok()?.into_iter().find(|(k, _)| k == "window_title").map(|(_, v)| v)) {
            out["text"] = json!(t);
        }
    } else if full {
        let text = tidy(&p.text);
        let n = text.chars().count();
        out["text"] = json!(text.chars().take(FULL_CHARS).collect::<String>());
        if n > FULL_CHARS { out["more"] = json!(n - FULL_CHARS); }
    } else {
        let (text, more) = snippet(&p.text, query_terms, LINE_CHARS);
        out["text"] = json!(text);
        if more > 0 { out["more"] = json!(more); }
    }
    if full {
        out["id"] = json!(p.id);
        out["merged"] = json!(m.rows.iter().filter(|r| r.id != p.id).map(|r| r.id.clone()).collect::<Vec<_>>());
        out["related_ids"] = json!(related(db, &p.id)?.into_iter().filter(|t| !m.rows.iter().any(|r| &r.id == t)).collect::<Vec<_>>());
    }
    Ok(out)
}
/// Whether the page with `hits` stays within the text bound.
fn fits(hits: &[Value]) -> bool {
    json!({"search_mode":"semantic","hits":hits,"next_cursor":"c000000"}).to_string().len() <= TEXT_BYTES
}
/// How long Heimdall's own spoken replies stay out of semantic search results: they are still in the agent's
/// conversation. Older replies stay searchable, labelled with speaker "Heimdall".
const OWN_REPLY_WINDOW: Duration = Duration::from_secs(3600);
fn own_recent_reply(row: &Row, cutoff_ns: u64) -> bool {
    row.speakers.iter().any(|s| s == "Heimdall") && row.start >= cutoff_ns
}
fn excerpt(mut record: Value, offset: usize, bytes: usize) -> (Value, Option<usize>) {
    let text = record["text"].as_str().unwrap_or("");
    let mut end = (offset + bytes).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < text.len()).then_some(end);
    let part = text[offset..end].to_owned();
    record["text"] = json!(part);
    record["excerpt"] = json!(offset > 0 || next.is_some());
    record["text_offset_bytes"] = json!(offset);
    (record, next)
}
/// Bytes of whole text per `id` page.
const DETAIL_BYTES: usize = 3000;
/// Recent chunks not embedded yet (in flight, or waiting in `embedding::LIVE_BUFFER` when it is set) are invisible to ranking.
/// The first semantic page adds up to this many whose text literally holds a query word, marked `pending_embedding`.
const PENDING_HITS: usize = 10;
fn pending_matches(db: &Connection, query: &Value) -> Result<Vec<String>> {
    let Some(text) = query["vector"]["query"].as_str() else { return Ok(vec![]) };
    use heimdall_archive::embedding::{LIVE_BUFFER, LIVE_BUFFER_MARGIN};
    let now = heimdall_archive::wall_ns();
    let mut since = now.saturating_sub((LIVE_BUFFER + LIVE_BUFFER_MARGIN).as_nanos() as u64);
    if let Some(start) = query["start"].as_str() { since = since.max(wall_ns(start)?); }
    let until = query["end"].as_str().map(wall_ns).transpose()?;
    let kinds: Vec<String> = query["kinds"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(str::to_owned)).collect();
    let tags: Vec<heimdall_archive::timeline::Tag> = serde_json::from_value(query.get("tags").cloned().unwrap_or(json!([])))?;
    let limit = PENDING_HITS.min(query["limit"].as_u64().unwrap_or(10) as usize);
    heimdall_archive::timeline::pending_literal(db, &heimdall_archive::embedding::model(), since, until, &heimdall_archive::timeline::literal_words(text), &kinds, &tags, limit)
}
pub async fn search(
    root: PathBuf,
    pages: Arc<Pages>,
    mut query: Value,
    permit: &DeliveryPermit,
) -> Result<Value> {
    let state = if let Some(cursor) = query["cursor"].as_str() {
        Some(pages.get(cursor, permit)?)
    } else {
        None
    };
    if let Some(h) = query["id"].as_str() {
        query["id"] = json!(pages.resolve(h)?);
    }
    let first_page = state.is_none();
    let permit = permit.clone();
    // Drop-to-interrupt uses the same archive mechanism as the legacy search.
    let cancellation = Arc::new(super::index::SearchCancellation::new());
    let _cancel = super::index::CancelSearch(cancellation.clone());
    tokio::task::spawn_blocking(move||->Result<Value>{
        permit.check()?;ensure!(heimdall_archive::store::exists(&root),"INDEX_UNAVAILABLE: timeline store is not ready; retry after indexing");
        let db=Connection::open_with_flags(root.join(heimdall_archive::store::INDEX),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;db.busy_timeout(Duration::from_secs(5))?;
        super::index::install_search_cancellation(&db,cancellation.clone(),permit.clone())?;*cancellation.active.lock().unwrap()=Some(db.get_interrupt_handle());
        db.execute_batch("BEGIN")?;
        let mut state=state.unwrap_or(Continuation{permit:permit.clone(),expires:Instant::now()+Duration::from_secs(3600),query:query.clone(),watermark:db.query_row("SELECT COALESCE(max(seq),0) FROM timeline_ingestion",[],|r|r.get(0))?,after:None,detail:None,ranked_offset:0,floor:None,shown:vec![]});
        if let Some(id)=state.query["id"].as_str(){if state.detail.is_none(){let exists=db.query_row("SELECT EXISTS(SELECT 1 FROM timeline_chunks WHERE id=?)",[id],|r|r.get::<_,bool>(0))?;ensure!(exists,"EVIDENCE_NOT_FOUND: use a handle or id returned by timeline_search");state.detail=Some((project(&db,id)?,0));}}
        if let Some((record,offset))=state.detail.take(){
            let mut header=record.clone();header["text"]=json!("");let metadata=header.to_string().len();ensure!(metadata<2048,"EVIDENCE_METADATA_TOO_LARGE: narrow source metadata");
            ensure!(record.to_string().len()<=4*1024*1024,"EVIDENCE_TOO_LARGE: use a bounded archive file read");let original=record.clone();let (mut record,next)=excerpt(record,offset,DETAIL_BYTES);
            record["h"]=json!(pages.handle(record["id"].as_str().unwrap_or("")));
            let cursor=if let Some(next)=next{state.detail=Some((original,next));Some(pages.put(state.clone())?)}else{None};
            let out=json!({"search_mode":"detail","anchor":state.watermark.to_string(),"hits":[record],"next_cursor":cursor});ensure!(out.to_string().len()<=TEXT_BYTES,"OUTPUT_TOO_LARGE: evidence metadata exceeds page bound");permit.check()?;return Ok(out);
        }
        let full=state.query["output"]=="full";
        let query_text=state.query["vector"]["query"].as_str().or(state.query["query"].as_str()).unwrap_or("").to_owned();
        let query_terms=terms(&query_text);
        let limit=state.query["limit"].as_u64().unwrap_or(10) as usize;
        if let Some(ranked)=state.query["_ranked"].as_array().cloned() {
            // The floor comes from the best row actually shown (never a skipped own reply); the fixed minimum is
            // EmbeddingGemma's, so a caller's own vector gets only the spread.
            let by_text=state.query["vector"]["query"].is_string();
            let recent_cutoff=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64-OWN_REPLY_WINDOW.as_nanos() as u64;
            let mut moments:Vec<Moment>=vec![]; let mut consumed=0; let mut exhausted=true;
            for row_ in ranked.iter().skip(state.ranked_offset) {
                let id=row_[0].as_str().context("invalid semantic result")?;
                let similarity=row_[1].as_f64();
                let r=row(&db,id)?;
                // Heimdall's own recent replies are already in the agent's conversation; as hits they outranked
                // what the user asked for (18 Oct 13:48: ranks 1-2 were its own reply from 12 minutes earlier).
                if own_recent_reply(&r,recent_cutoff) {consumed+=1;continue;}
                if state.shown.iter().any(|x|same_moment(x,&r)) {consumed+=1;continue;}
                if let Some(m)=moments.iter_mut().find(|m|m.rows.iter().any(|x|same_moment(x,&r))) {m.rows.push(r);consumed+=1;continue;}
                let floor=*state.floor.get_or_insert_with(||floor(similarity.unwrap_or(0.0),by_text));
                if similarity.is_some_and(|s|s<floor) {consumed=ranked.len()-state.ranked_offset;break;}
                if moments.len()==limit {exhausted=false;break;}
                moments.push(Moment{rows:vec![r],similarity}); consumed+=1;
            }
            let mut hits=vec![];
            for (i,m) in moments.iter().enumerate() {
                let h=hit(&db,&pages,m,&query_terms,full)?;
                let mut proposed=hits.clone(); proposed.push(h.clone());
                if !fits(&proposed) {ensure!(!hits.is_empty(),"OUTPUT_TOO_LARGE: semantic metadata exceeds page bound");
                    // The rest of this page goes to the next one: rewind to the first row of the first moment left.
                    let first_left=moments[i..].iter().flat_map(|m|m.rows.iter()).filter_map(|r|ranked.iter().skip(state.ranked_offset).position(|x|x[0]==r.id.as_str())).min().unwrap_or(consumed);
                    consumed=first_left; exhausted=false; break;}
                hits.push(h);
            }
            state.ranked_offset+=consumed;
            state.shown.extend(moments.iter().take(hits.len()).flat_map(|m|m.rows.iter().cloned()));
            if first_page {
                // Not embedded yet, so no similarity: after the ranked hits, merged into moments like them.
                let mut waiting:Vec<Moment>=vec![];
                for id in pending_matches(&db,&state.query)? {
                    let r=row(&db,&id)?;
                    if own_recent_reply(&r,recent_cutoff) || state.shown.iter().any(|x|x.id==r.id || same_moment(x,&r)) {continue;}
                    if let Some(m)=waiting.iter_mut().find(|m|m.rows.iter().any(|x|same_moment(x,&r))) {m.rows.push(r);continue;}
                    waiting.push(Moment{rows:vec![r],similarity:None});
                }
                let mut added=0;
                for m in &waiting {
                    let mut h=hit(&db,&pages,m,&query_terms,full)?; h["pending_embedding"]=json!(true);
                    let mut proposed=hits.clone(); proposed.push(h.clone());
                    if !fits(&proposed) {break;}
                    hits.push(h); added+=1;
                }
                state.shown.extend(waiting.iter().take(added).flat_map(|m|m.rows.iter().cloned()));
            }
            let more=!exhausted && state.ranked_offset<ranked.len();
            let cursor=if more{Some(pages.put(state.clone())?)}else{None};permit.check()?;
            return Ok(json!({"search_mode":"semantic","hits":hits,"next_cursor":cursor}));
        }
        let a=&state.query;
        // Rows of one moment come together, so a page reads several rows per distinct moment.
        let fetch=(limit*4).min(200);
        let mut sql="SELECT c.id,c.start_wall_ns FROM timeline_chunks c JOIN timeline_ingestion g ON g.chunk_id=c.id WHERE g.seq<=?".to_owned();let mut values:Vec<rusqlite::types::Value>=vec![state.watermark.into()];
        if let Some((wall,id))=&state.after{sql+=" AND (c.start_wall_ns<? OR (c.start_wall_ns=? AND c.id>?))";values.extend([(*wall).into(),(*wall).into(),id.clone().into()]);}
        if let Some(q)=a["query"].as_str(){sql+=" AND instr(lower(c.text),lower(?))>0";values.push(q.to_owned().into());}
        for (key,op) in [("start","c.end_wall_ns>=?"),("end","c.start_wall_ns<=?")]{if let Some(t)=a[key].as_str(){sql+=" AND ";sql+=op;values.push((wall_ns(t)? as i64).into());}}
        if let Some(kinds)=a["kinds"].as_array(){if !kinds.is_empty(){sql+=" AND c.kind IN (";sql+=&vec!["?";kinds.len()].join(",");sql+=")";for k in kinds{values.push(k.as_str().unwrap().to_owned().into());}}}
        for tag in a["tags"].as_array().into_iter().flatten(){sql+=" AND EXISTS(SELECT 1 FROM chunk_tags t WHERE t.chunk_id=c.id AND t.key=? AND t.value=?)";values.push(tag["key"].as_str().unwrap().to_owned().into());values.push(tag["value"].as_str().unwrap().to_owned().into());}
        sql+=" ORDER BY c.start_wall_ns DESC,c.id LIMIT ?";values.push((fetch as i64+1).into());
        let rows=db.prepare(&sql)?.query_map(rusqlite::params_from_iter(values),|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut moments:Vec<Moment>=vec![];let mut order:Vec<(usize,i64,String)>=vec![];let mut has_more=rows.len()>fetch;
        for (id,wall) in rows.iter().take(fetch){permit.check()?;let r=row(&db,id)?;
            if state.shown.iter().any(|x|same_moment(x,&r)){order.push((usize::MAX,*wall,id.clone()));continue;}
            if let Some(i)=moments.iter().position(|m|m.rows.iter().any(|x|same_moment(x,&r))){moments[i].rows.push(r);order.push((i,*wall,id.clone()));continue;}
            if moments.len()==limit{has_more=true;break;}
            moments.push(Moment{rows:vec![r],similarity:None});order.push((moments.len()-1,*wall,id.clone()));
        }
        let mut hits=vec![];let mut after=order.last().map(|(_,w,id)|(*w,id.clone())).or(state.after.clone());
        for (i,m) in moments.iter().enumerate(){
            let h=hit(&db,&pages,m,&query_terms,full)?;let mut proposed=hits.clone();proposed.push(h.clone());
            if !fits(&proposed){ensure!(!hits.is_empty(),"OUTPUT_TOO_LARGE: one evidence row exceeds page bound");
                // The next page starts at this moment's first row.
                let p=order.iter().position(|(m,_,_)|*m>=i&&*m!=usize::MAX).unwrap();
                after=Some((order[p-1].1,order[p-1].2.clone()));has_more=true;break;}
            hits.push(h);
        }
        state.after=after;
        state.shown.extend(moments.iter().take(hits.len()).flat_map(|m|m.rows.iter().cloned()));
        let cursor=if has_more{Some(pages.put(state.clone())?)}else{None};permit.check()?;
        Ok(json!({"search_mode":"literal","anchor":state.watermark.to_string(),"hits":hits,"next_cursor":cursor}))
    }).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::delivery::DeliveryGate;
    use std::sync::atomic::Ordering;
    fn insert(db: &Connection, id: &str, text: &str, wall: i64) -> Result<()> {
        db.execute("INSERT INTO timeline_chunks VALUES(?1,1,'text','utterance','mic',0,1,?2,?2,?3,'{}','hash')",params![id,wall,text])?;
        Ok(())
    }
    /// 18 Oct 13:48: a semantic search's top two hits were Heimdall's own reply from 12 minutes earlier, and the agent
    /// then misreported where the match came from.
    #[tokio::test]
    async fn semantic_hits_skip_heimdalls_own_recent_replies() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as i64;
        for (id, speaker, wall) in [("own-recent", "Heimdall", now - 12 * 60 * 1_000_000_000), ("owner-old", "User", now - 5 * 86_400 * 1_000_000_000), ("own-old", "Heimdall", now - 5 * 86_400 * 1_000_000_000)] {
            insert(&db, id, "compaction animation in the island", wall)?;
            db.execute("INSERT INTO chunk_tags(chunk_id,key,value) VALUES(?1,'speaker',?2)", params![id, speaker])?;
        }
        let gate = Arc::new(DeliveryGate::new(true));
        let out = search(root, Arc::new(Pages::default()), json!({"_ranked":[["own-recent",0.76],["owner-old",0.73],["own-old",0.72]],"limit":2,"output":"full"}), &gate.permit()?).await?;
        let ids: Vec<_> = out["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().to_owned()).collect();
        assert_eq!(ids, ["owner-old", "own-old"]);
        assert_eq!(out["hits"][1]["label"], "Mic · Heimdall", "older own replies stay, labelled");
        assert!(out["next_cursor"].is_null(), "all three candidates were consumed");
        Ok(())
    }
    /// live chunks may wait in a buffer before embedding. Until then semantic search still shows recent
    /// items whose text holds a query word, after the ranked hits, without a similarity and marked pending.
    #[tokio::test]
    async fn semantic_first_page_adds_recent_unembedded_literal_matches() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        let now = heimdall_archive::wall_ns() as i64;
        let minute = 60 * 1_000_000_000;
        insert(&db, "ranked", "an older note about the island", now - 3 * 86_400 * 1_000_000_000)?;
        insert(&db, "waiting", "the Gemma overhaul just landed", now - 2 * minute)?;
        insert(&db, "unrelated", "lunch plans", now - minute)?;
        insert(&db, "too-old", "gemma overhaul from yesterday", now - 86_400 * 1_000_000_000)?;
        let gate = Arc::new(DeliveryGate::new(true));
        let query = json!({"vector":{"query":"gemma overhaul"},"_ranked":[["ranked",0.71]],"limit":10,"output":"full"});
        let out = search(root.clone(), Arc::new(Pages::default()), query, &gate.permit()?).await?;
        let hits = out["hits"].as_array().unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["ranked", "waiting"]);
        assert_eq!(hits[0]["similarity"], 0.71);
        assert!(hits[0].get("pending_embedding").is_none());
        assert_eq!(hits[1]["pending_embedding"], true);
        assert!(hits[1].get("similarity").is_none());
        assert_eq!(hits[1]["label"], "Mic");
        // Raw vectors carry no text, so there is nothing to match literally.
        let raw = json!({"vector":{"model_id":"m","model_version":"1","values":[1.0]},"_ranked":[],"limit":10});
        let out = search(root, Arc::new(Pages::default()), raw, &gate.permit()?).await?;
        assert!(out["hits"].as_array().unwrap().is_empty());
        Ok(())
    }
    #[tokio::test]
    async fn byte_budget_detail_reconstruction_snapshot_and_caller_binding() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        let wall = 1_760_000_000_000_000_000i64;
        let text = "😀\n\"Zażółć\" ".repeat(2000);
        insert(&db, "long", &text, wall)?;
        // One minute apart: rows at the same moment of one source merge into one hit.
        for i in 0..25 {
            insert(&db, &format!("row-{i:02}"), "hello", wall - (i + 1) * 60_000_000_000)?;
        }
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit()?;
        let pages = Arc::new(Pages::default());
        let first = search(
            root.clone(),
            pages.clone(),
            json!({"query":"hello","limit":7,"output":"full"}),
            &permit,
        )
        .await?;
        assert_eq!(first["hits"].as_array().unwrap().len(), 7);
        insert(&db, "inserted-after", "hello", wall + 3_600_000_000_000)?;
        let mut ids = first["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let mut next = first["next_cursor"].clone();
        while !next.is_null() {
            let p = search(root.clone(), pages.clone(), json!({"cursor":next}), &permit).await?;
            assert!(p.to_string().len() <= TEXT_BYTES);
            ids.extend(
                p["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v["id"].as_str().unwrap().to_owned()),
            );
            next = p["next_cursor"].clone();
        }
        assert_eq!(ids.len(), 25);
        assert!(!ids.contains(&"inserted-after".into()));
        let unique = ids.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 25);
        let mut reconstructed = String::new();
        let mut q = json!({"id":"long"});
        loop {
            let p = search(root.clone(), pages.clone(), q, &permit).await?;
            assert!(p.to_string().len() <= TEXT_BYTES);
            reconstructed.push_str(p["hits"][0]["text"].as_str().unwrap());
            if p["next_cursor"].is_null() {
                break;
            }
            q = json!({"cursor":p["next_cursor"]});
        }
        assert_eq!(reconstructed, text);
        let other = Arc::new(DeliveryGate::new(true));
        assert!(search(
            root.clone(),
            pages.clone(),
            json!({"cursor":first["next_cursor"]}),
            &other.permit()?
        )
        .await
        .is_err());
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        assert!(search(
            root.clone(),
            pages,
            json!({"cursor":first["next_cursor"]}),
            &gate.permit()?
        )
        .await
        .is_err());
        Ok(())
    }
    #[tokio::test]
    async fn exact_tags_empty_and_missing_id_are_distinct() -> Result<()> {
        let d = tempfile::tempdir()?;
        let root = d.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        insert(&db, "a", "checkout", 1_760_000_000_000_000_000)?;
        db.execute("INSERT INTO chunk_tags VALUES('a','speaker','Alex')", [])?;
        let p = Arc::new(DeliveryGate::new(true)).permit()?;
        let pages = Arc::new(Pages::default());
        let hits = search(
            root.clone(),
            pages.clone(),
            json!({"tags":[{"key":"speaker","value":"Alex"}]}),
            &p,
        )
        .await?;
        assert_eq!(hits["hits"][0]["label"], "Mic · Alex");
        let empty = search(root.clone(), pages.clone(), json!({"query":"missing"}), &p).await?;
        assert!(empty["hits"].as_array().unwrap().is_empty());
        assert!(search(root, pages, json!({"id":"missing"}), &p)
            .await
            .unwrap_err()
            .to_string()
            .contains("EVIDENCE_NOT_FOUND"));
        Ok(())
    }
    fn tag(db: &Connection, id: &str, key: &str, value: &str) -> Result<()> {
        db.execute("INSERT INTO chunk_tags(chunk_id,key,value) VALUES(?1,?2,?3)", params![id, key, value])?;
        Ok(())
    }
    /// The raw page had 8 hits, 4 of them the same sentence (turn and utterance, text and audio).
    /// One moment is one line with a short label and a handle that `id` takes back; the freed slots go to the next
    /// distinct moments; hits under the similarity floor stay out.
    #[tokio::test]
    async fn one_moments_rows_merge_into_one_hit_with_a_handle_and_a_short_label() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        let wall = 1_760_000_000_000_000_000i64;
        let mic = "mic:AppleUSBAudioEngine:Shure Inc:Shure MV7i:MV7i#7-8bc6:2,3:29";
        let said = "No, it's fine. By the way, uh, the Iron Man 2 extract has the centre channel. Catch all the Jarvis lines from the movie and stitch them up.";
        for (id, kind) in [("56:speech_audio:turn:s-1:241", "speech_audio"), ("56:text:turn:s-1:241", "text"), ("56:speech_audio:utterance:s-1:241", "speech_audio"), ("56:text:utterance:s-1:241", "text")] {
            db.execute("INSERT INTO timeline_chunks VALUES(?1,1,?2,'utterance',?3,0,1,?4,?5,?6,'{}','hash')", params![id, kind, mic, wall, wall + 20_000_000_000, said])?;
            tag(&db, id, "speaker", "User")?;
        }
        db.execute("INSERT INTO timeline_chunks VALUES('15:moment:window:9',1,'moment','window','timeline',0,1,?1,?2,'Jarvis is the coolest AI','{}','hash')", params![wall - 86_400_000_000_000, wall - 86_355_000_000_000])?;
        db.execute("INSERT INTO chunk_links VALUES('15:moment:window:9','member','15:text:utterance:s-2:43')", [])?;
        db.execute("INSERT INTO timeline_chunks VALUES('15:speech_audio:utterance:s-2:43',1,'speech_audio','utterance','process:2359:17:2',0,1,?1,?1,'the Jarvis from The Iron Man','{}','hash')", params![wall - 86_380_000_000_000])?;
        tag(&db, "15:speech_audio:utterance:s-2:43", "speaker", "Mohawk")?;
        tag(&db, "15:speech_audio:utterance:s-2:43", "app", "com.hnc.Discord")?;
        insert(&db, "weak", "unrelated", wall - 1)?;
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit()?;
        let pages = Arc::new(Pages::default());
        let ranked = json!([["56:speech_audio:turn:s-1:241",0.82],["56:speech_audio:utterance:s-1:241",0.81],["15:speech_audio:utterance:s-2:43",0.80],["56:text:turn:s-1:241",0.79],["15:moment:window:9",0.79],["56:text:utterance:s-1:241",0.79],["weak",0.70]]);
        let out = search(root.clone(), pages.clone(), json!({"_ranked":ranked,"vector":{"query":"User talking about Jarvis voice from Iron Man"},"limit":10}), &permit).await?;
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2, "two distinct moments; the weak hit is under the floor: {out}");
        assert_eq!(hits[0]["label"], "Mic · User");
        assert_eq!(hits[1]["label"], "Discord · Mohawk", "the window merges with the line it contains");
        assert!(out["next_cursor"].is_null());
        let line = agent::plain::timeline(&out, &chrono::FixedOffset::east_opt(3600).unwrap());
        assert!(!line.contains("AppleUSB") && !line.contains("related") && !line.contains("excerpt"), "{line}");
        assert!(line.contains(" · Mic · User · 0.82 · [h1] ") && line.contains("Jarvis lines"), "{line}");
        assert!(!line.contains(" uh,"), "fillers stay out of the line: {line}");
        // The handle opens the whole text, fillers and all.
        let whole = search(root.clone(), pages.clone(), json!({"id":"h1"}), &permit).await?;
        assert_eq!(whole["hits"][0]["text"], said);
        assert!(search(root, pages, json!({"id":"h99"}), &permit).await.unwrap_err().to_string().contains("HANDLE_EXPIRED"));
        Ok(())
    }
    #[test]
    fn labels_snippets_and_app_names() {
        assert_eq!(app_name("com.citrolabs.ego.lite"), "Ego Lite");
        assert_eq!(app_name("com.hnc.Discord"), "Discord");
        assert_eq!(app_name("dev.heimdall.app"), "Heimdall");
        assert_eq!(label("display:1", "screen", Some("com.citrolabs.ego.lite"), &[]), "Screen · Ego Lite");
        assert_eq!(label("process:1:2:3", "speech_audio", Some("com.hnc.Discord"), &["Morgan".into()]), "Discord · Morgan");
        let long = format!("{} The Jarvis voice is the one we want. {}", "Filler words go here and there. ".repeat(10), "More words after it. ".repeat(10));
        let (text, more) = snippet(&long, &terms("Jarvis voice"), LINE_CHARS);
        assert_eq!(text, "The Jarvis voice is the one we want.");
        assert_eq!(more, tidy(&long).chars().count() - text.chars().count());
    }
    /// Review 26 Oct: a long sentence without spaces after the match (a path, CJK text) panicked in `cut`.
    #[test]
    fn a_snippet_of_text_without_spaces_never_panics() {
        let path = format!("a {}-jarvis{}", "x".repeat(100), "y".repeat(100));
        let (text, more) = snippet(&path, &terms("jarvis"), LINE_CHARS);
        assert!(text.chars().filter(|c| *c != '…').count() <= LINE_CHARS && more > 0, "{text}");
        let cjk = "贾维斯".repeat(80);
        assert!(!snippet(&cjk, &terms("贾维斯"), LINE_CHARS).0.is_empty());
    }
    /// Review 26 Oct: the floor came from rank 1 even when rank 1 was Heimdall's own skipped reply, so the real match
    /// fell under it; and a moment's later row came back as a new hit on the next page.
    #[tokio::test]
    async fn the_floor_ignores_skipped_replies_and_pages_never_repeat_a_moment() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("archive");
        let db = heimdall_archive::store::open(&root)?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as i64;
        let day = 86_400_000_000_000i64;
        insert(&db, "own", "an answer", now - 60_000_000_000)?;
        tag(&db, "own", "speaker", "Heimdall")?;
        for (i, id) in ["56:text:utterance:s:1", "56:text:utterance:s:2", "56:speech_audio:utterance:s:1"].iter().enumerate() {
            let wall = now - day * if id.ends_with(":2") { 2 } else { 1 };
            db.execute("INSERT INTO timeline_chunks VALUES(?1,1,'text','utterance',?2,0,1,?3,?3,'the words','{}','hash')", params![id, format!("mic:{i}"), wall])?;
        }
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit()?;
        let pages = Arc::new(Pages::default());
        let ranked = json!([["own",0.88],["56:text:utterance:s:1",0.80],["56:text:utterance:s:2",0.79],["56:speech_audio:utterance:s:1",0.78]]);
        let first = search(root.clone(), pages.clone(), json!({"_ranked":ranked,"vector":{"query":"words"},"limit":1}), &permit).await?;
        assert_eq!(first["hits"].as_array().unwrap().len(), 1, "{first}");
        let second = search(root.clone(), pages.clone(), json!({"cursor":first["next_cursor"]}), &permit).await?;
        let shown: Vec<_> = second["hits"].as_array().unwrap().iter().map(|h| h["h"].clone()).collect();
        assert_eq!(shown.len(), 1, "s:2 only; s:1's audio row is the moment already shown: {second}");
        assert_ne!(shown[0], first["hits"][0]["h"]);
        assert!(second["next_cursor"].is_null());
        Ok(())
    }
}
