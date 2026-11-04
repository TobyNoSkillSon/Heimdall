//! Settled, written by the shadow ("the shadow job that runs Goals should also write Settled"; Settled is
//! proof of progress, drawn as day columns). One item per finished piece of goal work (a Goals task newly done:
//! `shipped`) or decision of the user's that moves a goal (`decided`), each with `at`: the epoch second of the line that
//! shows it, never a word like "Today" (the plan refresher's "Today" was never turned into a date, so every row read
//! Today for days). Code does what code can: the date, the dedupe, the goal check, and items the user removed never come
//! back (`removed`, written by `plan_edit` remove); the model only judges what happened.

use crate::goals::ops::{normalise, Evidence, Op};
use crate::islands::files::Root;
use serde_json::{json, Map, Value};

pub const ID: &str = "settled";
pub const TITLE: &str = "Settled";
pub const ICON: &str = "checkmark.seal";
/// Items kept on disk, newest kept (the island shows a week and counts older ones).
pub const KEEP: usize = 300;
/// Removed titles remembered.
pub const REMOVED_KEEP: usize = 300;
/// An item counts as one already there when their titles share this share of content words (or one names the same task).
pub const SAME_SHARE: f64 = 0.75;
/// What the user removed matches more loosely (half the shorter title's content words): a removal is a strong no.
pub const REMOVED_SHARE: f64 = 0.6;
/// A removal holds this long (later work on the same thing is new work).
pub const REMOVED_DAYS: i64 = 14;
/// Within this many seconds: the same work told twice (a worker's notice, then Heimdall telling the user).
pub const SAME_WINDOW: i64 = 3 * 86_400;
/// Decisions settled from one turn, at most.
pub const DECISIONS: usize = 2;
/// A Settled title, at most (two lines of the island's row).
pub const TITLE_CHARS: usize = 64;

/// One entry to record.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub title: String,
    /// `shipped` or `decided`.
    pub kind: &'static str,
    pub goal: String,
    /// The Goals task it records (shipped), else "".
    pub task: String,
    /// Epoch seconds of the line that shows it.
    pub at: i64,
    pub quote: String,
    /// What proves it (`goals::proof`): a commit, test, install or file on its line, or the user's quote for a decision.
    /// `None`: claimed (drawn dashed, never counted).
    pub proof: Option<Value>,
}

/// The whole line (stream entry) a quote came from: the newest holding it.
pub fn line_of<'a>(quote: &str, lines: &'a Evidence) -> Option<&'a str> {
    let q = normalise(quote);
    if q.chars().count() < crate::goals::ops::QUOTE_MIN { return None }
    lines.times.iter().rev().find(|(t, _)| normalise(t).contains(&q)).map(|(t, _)| t.as_str())
}

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

const STOP: [&str; 40] = ["a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "by", "at", "from", "is", "are", "be",
    "it", "its", "this", "that", "now", "new", "done", "ready", "first", "into", "as", "up", "out", "all", "one", "two", "his", "our",
    "we", "so", "no", "not", "via"];

/// Content words, lightly stemmed (`shipped` and `ships` are `ship`).
pub fn words(t: &str) -> Vec<String> {
    let mut out: Vec<String> = normalise(t).split(' ').filter(|w| !w.is_empty() && !STOP.contains(w)).map(|w| {
        let mut w = w.to_owned();
        for suf in ["ing", "ed", "es", "s"] {
            // "-es" only after s, x, z, ch, sh ("fixes"); otherwise the "s" alone ("candidates" is "candidate").
            if suf == "es" && !["ses", "xes", "zes", "ches", "shes"].iter().any(|e| w.ends_with(e)) { continue }
            if w.len() > suf.len() + 2 && w.ends_with(suf) {
                w.truncate(w.len() - suf.len());
                let b = w.as_bytes();
                if (suf == "ing" || suf == "ed") && b.len() > 3 && b[b.len() - 1] == b[b.len() - 2] && !b"aeiouls".contains(&b[b.len() - 1]) { w.pop(); }
                break;
            }
        }
        w
    }).collect();
    out.sort();
    out.dedup();
    out
}

/// Two titles name the same thing: equal once normalised, or sharing `SAME_SHARE` of the shorter's content words
/// (at least two words when both have three or more).
pub fn same(a: &str, b: &str) -> bool { overlap(a, b, SAME_SHARE) }

/// Equal once normalised, or sharing `share` of the shorter title's content words (two at least when both have three).
pub fn overlap(a: &str, b: &str, share: f64) -> bool {
    if normalise(a) == normalise(b) { return true }
    let (x, y) = (words(a), words(b));
    if x.is_empty() || y.is_empty() { return false }
    let shared = x.iter().filter(|w| y.contains(w)).count();
    let short = x.len().min(y.len());
    shared as f64 >= share * short as f64 && (shared >= 2 || short < 3)
}

/// The time of the line a quote comes from (epoch s): the newest line holding it, else `None`.
pub fn when(quote: &str, lines: &Evidence) -> Option<i64> {
    let q = normalise(quote);
    if q.chars().count() < crate::goals::ops::QUOTE_MIN { return None }
    lines.times.iter().rev().find(|(t, _)| normalise(t).contains(&q)).map(|(_, at)| *at)
}

/// What Settled starts as (docked left, under Scheduled).
pub fn fresh() -> Map<String, Value> {
    json!({"id": ID, "preset": "todo", "list": ID, "title": TITLE, "icon": ICON, "side": "left", "order": 2, "items": []})
        .as_object().unwrap().clone()
}

/// Two quotes tell the same thing: one holds the other, they share a run of five words, or one shares 60 % of the
/// other's content words (the lab's checker counts a removed item back by the same rule).
fn same_quote(a: &str, b: &str) -> bool {
    let (na, nb) = (normalise(a), normalise(b));
    if na.chars().count() < crate::goals::ops::QUOTE_MIN || nb.chars().count() < crate::goals::ops::QUOTE_MIN { return false }
    if (na.split(' ').count() >= 5 && nb.contains(&na)) || (nb.split(' ').count() >= 5 && na.contains(&nb)) { return true }
    let ta: Vec<&str> = na.split(' ').collect();
    let tb: Vec<&str> = nb.split(' ').collect();
    if ta.len() >= 5 && tb.windows(5).any(|w| ta.windows(5).any(|v| v == w)) { return true }
    let (wa, wb) = (words(a), words(b));
    let shared = wa.iter().filter(|w| wb.contains(w)).count();
    !wa.is_empty() && !wb.is_empty() && shared >= 3 && shared as f64 >= SAME_SHARE * wa.len().min(wb.len()) as f64
}

/// Something the user removed (from Settled, or an ask from Goals) names this title, or came from this quote: never
/// written again, however the model words it next time.
pub fn removed_like(file: &Map<String, Value>, title: &str, quote: &str, now: i64) -> bool {
    removed_in(file.get("removed").and_then(Value::as_array).map_or(&[][..], Vec::as_slice), title, quote, now)
}

/// `removed_like` over a `removed` array.
/// Review 2 Nov: the project prefix ("Arsenal:") is not what was removed; one shared word never is; removals expire
/// after `REMOVED_DAYS` (`at` 0, an undated seed, never does).
pub fn removed_in(list: &[Value], title: &str, quote: &str, now: i64) -> bool {
    let body = |t: &str| t.split_once(':').map_or(t, |(_, b)| b).to_owned();
    list.iter().any(|r| {
        let at = r.get("at").and_then(Value::as_i64).unwrap_or(0);
        if at > 0 && now - at > REMOVED_DAYS * 86_400 { return false }
        let t = r.get("title").and_then(Value::as_str).or_else(|| r.as_str()).unwrap_or("");
        let q = r.get("quote").and_then(Value::as_str).unwrap_or("");
        let shared = { let (a, b) = (words(&body(t)), words(&body(title))); a.iter().filter(|w| b.contains(w)).count() };
        (!t.is_empty() && shared >= 2 && overlap(&body(t), &body(title), REMOVED_SHARE)) || (!q.is_empty() && !quote.is_empty() && same_quote(q, quote))
    })
}

/// A title the user removed.
pub fn removed(file: &Map<String, Value>, title: &str) -> bool { removed_like(file, title, "", 0) }

/// Remembers a removed item (its title and the quote it came from) in `file` (newest `REMOVED_KEEP`).
pub fn remember_removed(file: &mut Map<String, Value>, title: &str, quote: &str, now: i64) {
    if title.trim().is_empty() || removed(file, title) { return }
    let r = file.entry("removed").or_insert_with(|| json!([]));
    if !r.is_array() { *r = json!([]) }
    let a = r.as_array_mut().unwrap();
    a.push(json!({"title": title, "quote": quote, "at": now}));
    let over = a.len().saturating_sub(REMOVED_KEEP);
    a.drain(..over);
}

/// Why `e` is not written, or `None`: it serves no goal, the user removed it, or it is already there (the same task, or the
/// same title within `SAME_WINDOW`).
pub fn refuse(file: &Map<String, Value>, e: &Entry) -> Option<String> {
    if e.goal.is_empty() { return Some("serves no goal".into()) }
    if e.title.trim().is_empty() { return Some("no title".into()) }
    if removed_like(file, &e.title, &e.quote, e.at) { return Some("User removed it".into()) }
    let items = file.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object);
    for i in items {
        if !e.task.is_empty() && s(i, "task") == e.task { return Some("already settled (same task)".into()) }
        let at = i.get("at").and_then(Value::as_i64);
        let near = at.is_none_or(|a| (a - e.at).abs() <= SAME_WINDOW);
        if near && same(s(i, "title"), &e.title) { return Some(format!("already settled ({})", s(i, "title"))) }
    }
    None
}

/// Adds the entries `refuse` lets through to `file`; returns one reason per entry ("" when written).
pub fn add(file: &mut Map<String, Value>, entries: &[Entry]) -> Vec<String> {
    let mut out = Vec::new();
    let mut decided = 0;
    for e in entries {
        if let Some(r) = refuse(file, e) { out.push(r); continue }
        // At most `DECISIONS` a turn (counted after dedupe): a decision sets a goal's direction; a run of instructions
        // is the work itself.
        if e.kind == "decided" { if decided >= DECISIONS { out.push(format!("over {DECISIONS} decisions this turn")); continue } decided += 1 }
        let items = file.entry("items").or_insert_with(|| json!([]));
        if !items.is_array() { *items = json!([]) }
        let a = items.as_array_mut().unwrap();
        let id = format!("s-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        a.push(json!({"id": id, "title": crate::goals::ops::cap(&e.title, TITLE_CHARS), "done": true, "at": e.at,
                      "kind": e.kind, "goal": e.goal, "task": e.task, "by": crate::goals::ops::BY, "quote": crate::goals::ops::cap(&e.quote, 200)}));
        if let (Some(p), Some(last)) = (&e.proof, a.last_mut()) { last["proof"] = p.clone() }
        // Oldest first on disk, by when it happened; the newest `KEEP` stay.
        a.sort_by_key(|v| v.get("at").and_then(Value::as_i64).unwrap_or(0));
        let over = a.len().saturating_sub(KEEP);
        a.drain(..over);
        out.push(String::new());
    }
    out
}

/// The entries a turn's ops make, against the Goals file before and after: each task newly done (its title, its goal,
/// the time of its quote's line) and each kept `decided` op.
pub fn entries(before: &Map<String, Value>, after: &Map<String, Value>, outcomes: &[crate::goals::ops::Outcome], lines: &Evidence, now: i64) -> Vec<Entry> {
    let kept: Vec<&Op> = outcomes.iter().filter(|o| o.kept).map(|o| &o.op).collect();
    // The op that moved each task (its reason names the task: "t-… done"), whatever title it gave.
    let mover = |id: &str| outcomes.iter().find(|o| o.kept && o.op.op == "progress" && o.reason == format!("{id} done")).map(|o| &o.op);
    let done_ids = |f: &Map<String, Value>| -> Vec<String> {
        f.get("goals").and_then(Value::as_array).into_iter().flatten()
            .flat_map(|g| g["tasks"].as_array().cloned().unwrap_or_default())
            .filter(|t| t["state"] == "done").filter_map(|t| t["id"].as_str().map(str::to_owned)).collect()
    };
    let was = done_ids(before);
    let mut out = Vec::new();
    for g in after.get("goals").and_then(Value::as_array).into_iter().flatten() {
        let gid = g["id"].as_str().unwrap_or("");
        for t in g["tasks"].as_array().into_iter().flatten().filter(|t| t["state"] == "done") {
            let id = t["id"].as_str().unwrap_or("");
            if id.is_empty() || was.iter().any(|w| w == id) { continue }
            let title = t["title"].as_str().unwrap_or("");
            // The op that did it: its quote and its line's time.
            let quote = mover(id).map(|o| o.quote.clone()).unwrap_or_default();
            // Settled's rows wrap to two lines: the op's own title, uncut up to `TITLE_CHARS` (Goals cuts at 40).
            let title = mover(id).map(|o| o.title.trim()).filter(|t| !t.is_empty() && t.chars().count() <= TITLE_CHARS).unwrap_or(title);
            let at = t["at"].as_i64().or_else(|| when(&quote, lines)).unwrap_or(now);
            // Its proof is on the line it came from, read by code; a task the user ticked is his own word.
            let proof = line_of(&quote, lines).and_then(|l| crate::goals::proof::near(l, &quote, at))
                .or_else(|| (t["ticked"] == true).then(|| crate::goals::proof::quote("ticked done by User", at)));
            out.push(Entry { title: title.into(), kind: "shipped", goal: gid.into(), task: id.into(), at, quote, proof });
        }
    }
    for o in kept.iter().filter(|o| o.op == "decided") {
        let at = when(&o.quote, lines).unwrap_or(now);
        // A decision is the user's (checked in `ops::needs_owner`): his words are its proof.
        out.push(Entry { title: crate::goals::ops::cap(&o.title, TITLE_CHARS), kind: "decided", goal: o.goal.clone(), task: String::new(), at, quote: o.quote.clone(),
                         proof: Some(crate::goals::proof::quote(&o.quote, at)) });
    }
    out
}

/// Writes `entries` into `islands/settled.json` (the caller holds the folder lock). Returns the reasons (see `add`).
pub fn record_locked(root: &Root, entries: &[Entry], now: i64) -> anyhow::Result<Vec<String>> {
    if entries.is_empty() { return Ok(Vec::new()) }
    let mut file = crate::goals::ops::current(root, ID).unwrap_or_else(fresh);
    let before = file.clone();
    let out = add(&mut file, entries);
    if file != before {
        file.insert("updated".into(), json!(now));
        crate::goals::ops::write_list(root, ID, &file)?;
    }
    Ok(out)
}
