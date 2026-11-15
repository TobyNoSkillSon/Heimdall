//! Lane R (night 11–12 Nov), bug note 2026-11-11T12-05-15: a job's report outlives its island. 
//! "if there was a report for the job, then the report just stays even though the island is actually being closed."
//!
//! A done job island leaves by itself (its thread archived, or its two minutes on screen, `binding::Retire`). When it
//! delivered a file (its `open` target, else its picture, else the file its final reply named: `named_in`, set as its
//! `open` by `binding::sync_patch`) and the user has not opened that file from it since it finished, the island is not
//! removed: it becomes a report island in the same file, so the helper turns it in place rather than popping one out
//! and another in. A report island is the job's title and icon, the green check (`preset: "result"`) and its open
//! button, unbound (no `thread`: the binding, the island agent and the plan refresher leave it alone). It stays until
//! The user opens it (`opened`) or closes it; at most `KEPT_AT_MOST` stay, the oldest going first. A job with no
//! deliverable goes as before. Every closed job's deliverable is also written to Settled as its proof (`settle`), so
//! today's reports are found again there; the Goals job's own row for the same work gets the proof instead of a twin.
//! The viewer is its own island (`view-…`): a report the user is reading stays open whatever its job island does.

use super::files::Root;
use serde_json::{json, Map, Value};
// Kept in goals/settled.rs so the goals lab, which includes that file alone, builds (night 1112 check).
pub use crate::goals::settled::{same_job_work as same_work, JOB_BY as BY};

/// Report islands on screen at most; a newer one sends the oldest away (it is still in Settled).
pub const KEPT_AT_MOST: usize = 3;
/// What a final reply's path must end in to be its deliverable: pages first (`named_in`), then documents, pictures,
/// sound and film, tables.
const PAGES: [&str; 2] = ["html", "htm"];
const KINDS: [&str; 17] = ["pdf", "md", "png", "jpg", "jpeg", "gif", "webp", "heic", "svg", "mp4", "mov", "m4v", "mp3", "wav", "m4a", "flac", "csv"];

fn s<'a>(o: &'a Map<String, Value>, k: &str) -> &'a str { o.get(k).and_then(Value::as_str).unwrap_or("") }

fn ext(p: &str) -> String {
    let name = p.rsplit('/').next().unwrap_or("");
    name.rsplit_once('.').map(|(stem, e)| if stem.is_empty() { String::new() } else { e.to_ascii_lowercase() }).unwrap_or_default()
}

/// Absolute paths (`/…`, `~/…`, `file://…`) in `text`, in order, `~` expanded and `%xx` decoded. A path ends at a
/// space or a closing mark; a sentence's last `.`, `:`, `!` or `?` is not part of it. Paths with spaces are not found.
fn paths(text: &str) -> Vec<String> {
    let stop = |c: char| c.is_whitespace() || "`\"'<>()[]{},;|*".contains(c);
    text.split(stop).filter_map(|w| {
        let w = w.trim_end_matches(['.', ':', '!', '?']);
        let w = w.strip_prefix("file://").unwrap_or(w);
        (w.starts_with('/') && !w.starts_with("//") || w.starts_with("~/")).then(|| super::report_link::absolute(w))
    }).collect()
}

/// A path a final reply names as its input or a reference, not as what it made: a bug note, an instruction or skill
/// file, a source or doc in a repository, the app's own data. Night 1112 fresh review: "Fixes bug note
/// …/bugs/notes/x.md" made a report island and a false Settled row.
fn incidental(p: &str) -> bool {
    const DIRS: [&str; 9] = ["/bugs/", "/.claude/", "/System/", "/skills/", "/docs/", "/crates/", "/src/", "/Library/", "/.git/"];
    const NAMES: [&str; 5] = ["AGENTS.md", "CLAUDE.md", "README.md", "SKILL.md", "CHANGELOG.md"];
    let name = p.rsplit('/').next().unwrap_or("");
    DIRS.iter().any(|d| p.contains(d)) || NAMES.contains(&name)
}

/// The deliverable `text` names (not checked on disk): its first page, else its first document, picture, recording,
/// film or table, leaving out inputs and references (`incidental`).
pub fn named_in(text: &str) -> Option<String> {
    let found: Vec<String> = paths(text).into_iter().filter(|p| !incidental(p)).collect();
    found.iter().find(|p| PAGES.contains(&ext(p).as_str()))
        .or_else(|| found.iter().find(|p| KINDS.contains(&ext(p).as_str())))
        .cloned()
}

/// `target` as a file on this Mac that exists (not a folder, not a web address), absolute.
pub fn file_of(target: &str) -> Option<String> {
    let t = target.trim();
    let t = t.strip_prefix("file://").unwrap_or(t);
    if !(t.starts_with('/') || t.starts_with("~/")) { return None }
    let p = super::report_link::absolute(t);
    std::path::Path::new(&p).is_file().then_some(p)
}

/// The island's open target may take the file its final reply named: it has none, or names a local file that is
/// not there (a page bound from the brief that was written elsewhere). A web address or a file that exists stays.
pub fn open_free(o: &Map<String, Value>) -> bool {
    let t = s(o, "open").trim();
    let t = t.strip_prefix("file://").unwrap_or(t);
    t.is_empty() || ((t.starts_with('/') || t.starts_with("~/")) && file_of(t).is_none())
}

/// The file a job island delivered: its open target, else its picture.
pub fn deliverable(o: &Map<String, Value>) -> Option<String> {
    file_of(s(o, "open")).or_else(|| file_of(s(o, "image")))
}

/// A report island (`keep`).
pub fn is_report(o: &Map<String, Value>) -> bool { o.get("report").is_some_and(Value::is_object) }

/// The report island a done job island `o` becomes: its title, icon and side, the green check, the file as its open
/// button, closed, and where it came from.
pub fn report_island(id: &str, o: &Map<String, Value>, file: &str, thread: &str, now: i64) -> Map<String, Value> {
    let mut r = Map::new();
    r.insert("id".into(), json!(id));
    r.insert("preset".into(), json!("result"));
    let title = Some(s(o, "title")).filter(|t| !t.trim().is_empty()).unwrap_or("Report");
    r.insert("title".into(), json!(title));
    for k in ["icon", "side", "goal", "position", "edgeY", "order"] {
        if let Some(v) = o.get(k).filter(|v| !v.is_null()) { r.insert(k.into(), v.clone()); }
    }
    r.insert("open".into(), json!(file));
    r.insert("expanded".into(), json!(false));
    r.insert("report".into(), json!({"thread": thread, "at": now}));
    r
}

/// A done job island `id` (`o`, read under the folder's lock, which the caller holds) leaves: it becomes a report
/// island when it delivered a file the user has not opened from it since it finished, else its file is removed. Returns
/// the file it delivered (for `settled`), kept or not.
pub fn leave_locked(root: &Root, id: &str, o: &Map<String, Value>, thread: &str, now: i64) -> anyhow::Result<Option<String>> {
    let file = deliverable(o);
    match &file {
        Some(f) if o.get("opened") != Some(&Value::Bool(true)) => {
            super::files::write_atomic(&root.file(id), &Value::Object(report_island(id, o, f, thread, now)))?;
        }
        _ => {
            if let Err(e) = std::fs::remove_file(root.file(id)) {
                if e.kind() != std::io::ErrorKind::NotFound { return Err(e.into()) }
            }
        }
    }
    Ok(file)
}

/// After `leave_locked`: the oldest report islands beyond `KEPT_AT_MOST` go, and the job's file is written to Settled.
pub fn settled(root: &Root, o: &Map<String, Value>, file: &str, now: i64) {
    if let Err(e) = root.locked(|| {
        let mut kept: Vec<(i64, String)> = root.all().into_iter().filter(|(_, r)| is_report(r))
            .map(|(id, r)| (r["report"]["at"].as_i64().unwrap_or(0), id)).collect();
        kept.sort();
        let over = kept.len().saturating_sub(KEPT_AT_MOST);
        for (_, id) in &kept[..over] { root.remove(id)?; }
        Ok(())
    }) {
        eprintln!("islands: older report islands not cleared: {}", super::short_error(&e));
    }
    if let Err(e) = settle(root, s(o, "title"), s(o, "goal"), file, now) {
        eprintln!("islands: report {file} not written to Settled: {}", super::short_error(&e));
    }
}

/// The user opened something from island `id`: a report island has done its job and goes; a done job island remembers
/// it, so its file is not kept on screen when it leaves (he has seen it; the viewer is its own island).
pub fn opened(root: &Root, id: &str) -> anyhow::Result<()> {
    root.locked(|| {
        let Some(mut o) = root.read(id) else { return Ok(()) };
        if is_report(&o) {
            root.remove(id)?;
        } else if s(&o, "state") == "done" && o.get("opened") != Some(&Value::Bool(true)) {
            o.insert("opened".into(), json!(true));
            super::files::write_atomic(&root.file(id), &Value::Object(o))?;
        }
        Ok(())
    })
}

/// The job of `thread` is back (a new task island for it): its report island goes, its island carries the file again.
pub fn job_back(root: &Root, thread: &str) {
    let gone = root.locked(|| {
        for (id, o) in root.all() {
            if is_report(&o) && o["report"]["thread"].as_str() == Some(thread) { root.remove(&id)?; }
        }
        Ok(())
    });
    if let Err(e) = gone { eprintln!("islands: report island of {thread} not removed: {}", super::short_error(&e)) }
}


/// The file proof of a closed job's deliverable.
pub fn proof(file: &str, at: i64) -> Value {
    json!({"kind": "file", "value": file, "label": file.rsplit('/').next().unwrap_or(file), "at": at})
}

/// Writes a closed job's file to Settled: as the proof of the Goals job's row for the same work when it has none,
/// else as its own row (the job's title, `by` `BY`). Nothing when a row already holds the file or the user removed it.
pub fn settle(root: &Root, title: &str, goal: &str, file: &str, now: i64) -> anyhow::Result<()> {
    let title = if title.trim().is_empty() { file.rsplit('/').next().unwrap_or(file) } else { title.trim() };
    root.locked(|| {
        let mut list = crate::goals::ops::current(root, crate::goals::settled::ID).unwrap_or_else(crate::goals::settled::fresh);
        if crate::goals::settled::removed_like(&list, title, "", now) { return Ok(()) }
        let items = list.entry("items").or_insert_with(|| json!([]));
        if !items.is_array() { *items = json!([]) }
        let a = items.as_array_mut().unwrap();
        if a.iter().any(|i| i["proof"]["kind"] == "file" && i["proof"]["value"] == file) { return Ok(()) }
        let twin = a.iter_mut().rev().find(|i| i["proof"]["kind"] != "file" && i["by"] != BY
            && same_work(i["title"].as_str().unwrap_or(""), i["at"].as_i64().unwrap_or(0), title, now));
        match twin {
            // The file is what the user opens again; another proof the row had (a commit, a test, his quote) is kept beside it.
            Some(i) => {
                let before = i["proof"].take();
                if !before.is_null() { i["alsoProof"] = before }
                i["proof"] = proof(file, now);
            }
            None => {
                let id = format!("s-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
                a.push(json!({"id": id, "title": crate::goals::ops::cap(title, crate::goals::settled::TITLE_CHARS), "done": true, "at": now,
                              "kind": "shipped", "goal": goal, "task": "", "by": BY, "quote": "", "proof": proof(file, now)}));
                a.sort_by_key(|v| v.get("at").and_then(Value::as_i64).unwrap_or(0));
                let over = a.len().saturating_sub(crate::goals::settled::KEEP);
                a.drain(..over);
            }
        }
        list.insert("updated".into(), json!(now));
        crate::goals::ops::write_list(root, crate::goals::settled::ID, &list)
    })
}


#[cfg(test)]
#[path = "kept_report_tests.rs"]
mod tests;
