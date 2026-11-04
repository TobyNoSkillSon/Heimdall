//! A bound thread as a model reads it (moved from the plan refresher, 30 Oct: the island agent reads threads now, the
//! refresher no longer does): its recent activity as plain lines, newest work first, its sub-agents and background
//! work as one job, and the pictures it produced. The binding keeps each bound thread's latest items in memory
//! (`binding::T3Source`), so building this costs no T3 call.

use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Pictures offered per thread (newest first).
pub const PICTURES_CAP: usize = 8;
/// The rendered thread's budget in characters (about 1.5k tokens).
pub const THREAD_BUDGET: usize = 6_000;

/// A bound thread as T3 answers about it: `t3_thread_read` (activity view) of its latest items, its sub-agents'
/// `t3_thread_list` entries, and the latest items of the ones still working (sub-agent id, activity answer).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadRaw {
    pub read: Value,
    pub children: Vec<Value>,
    pub child_reads: Vec<(String, Value)>,
    /// The sub-agent lookup answered (`children` is the whole truth).
    pub listed: bool,
}

/// A bound thread for one pass: its recent activity as plain lines, the pictures it produced (real image files,
/// newest first), whether it or a sub-agent still works, and a stamp that changes when the thread moves.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadInput {
    pub text: String,
    pub pictures: Vec<String>,
    pub working: bool,
    pub stamp: u64,
    /// T3's status of the thread's own turn ("running", "completed", …).
    pub status: String,
    /// The thread asks the user something (T3 "waiting" or a pending request): the binding shows that.
    pub asking: bool,
    /// Its sub-agents were listed this pass (a failed lookup says nothing about whether they work).
    pub listed: bool,
    /// Its own turn ended within the hour (the binding leaves the job's state to the refresher in that hour).
    pub ended_recently: bool,
}

/// Still working: changed within `CHILD_QUIET_S`, or changed within the hour and its latest item (thinking and
/// checkpoints aside) is not a closing message: it is waiting on something it started.
fn child_working(k: &Value, read: Option<&Value>, now: chrono::DateTime<chrono::Utc>) -> bool {
    if crate::islands::binding::child_working(k, now) { return true }
    let Some(r) = read.filter(|_| crate::islands::binding::recent_child(k, now)) else { return false };
    let last = r["items"].as_array().into_iter().flatten().rev().find(|it| !matches!(it["type"].as_str(), Some("reasoning" | "checkpoint")));
    last.is_some_and(|it| it["type"] != "assistant_message")
}

/// Background commands the latest run started within the hour and has not reported finished: the tool's own notice
/// ("Command running in background with ID", "was moved to the background") in its result, not the command line,
/// on an item of the latest run (both run ids known). A later "background task(s) finished/failed/completed" ends them.
pub fn background_commands<'a>(items: &[&'a Value], latest_run: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Vec<&'a Value> {
    let Some(latest) = latest_run else { return Vec::new() };
    let mut out: Vec<&Value> = Vec::new();
    for it in items.iter().copied().filter(|it| it["runId"].as_str() == Some(latest)) {
        let text = it["text"].as_str().unwrap_or("");
        let lower = text.to_ascii_lowercase();
        if lower.contains("background task") && ["finished", "completed", "failed", "killed"].iter().any(|w| lower.contains(w)) {
            out.clear();
            continue;
        }
        let notice = text.lines().skip(1).any(|l| l.contains("Command running in background with ID") || l.contains("was moved to the background"));
        let recent = it["updatedAt"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| (now - t.with_timezone(&chrono::Utc)).num_minutes() < 60);
        if it["type"] == "command_execution" && notice && recent { out.push(it); }
    }
    out
}

/// "Arsenal website rebuild [heimdall:ec97…]" → "Arsenal website rebuild".
pub fn without_tag(title: &str) -> &str {
    match title.rfind(" [heimdall:") { Some(i) if title.ends_with(']') => &title[..i], _ => title }
}

fn local_hhmm(ts: &Value) -> String {
    ts.as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

pub fn one_line(t: &str) -> String { t.split_whitespace().collect::<Vec<_>>().join(" ") }

/// One activity item as a line: what was asked or said, which command ran, which file was written, which tool was
/// used. Thinking and bookkeeping items are left out.
pub fn activity_line(it: &Value) -> Option<String> {
    let when = local_hhmm(&it["updatedAt"]);
    let text = it["text"].as_str().unwrap_or("");
    let first = text.lines().next().unwrap_or("").trim();
    let failed = if it["status"] == "failed" { " (failed)" } else { "" };
    let line = match it["type"].as_str()? {
        "user_message" => format!("asked: {}", clip(&one_line(text), 300)),
        "assistant_message" => format!("said: {}", clip(&one_line(text), 300)),
        "command_execution" if it["title"] == "Workspace ready" => return None,
        "command_execution" => format!("ran{failed}: {}", clip(first.trim_start_matches("$ "), 140)),
        "file_change" => format!("wrote {}", clip(first, 160)),
        "dynamic_tool" => {
            let name = it["title"].as_str().map(str::to_owned).or_else(|| {
                let at = text.find("\"toolName\": \"")? + 13;
                Some(text[at..].split('"').next()?.to_owned())
            })?;
            // What it ran: the model, operation or file the call names, and whether it is still going.
            let field = |k: &str| { let at = text.find(&format!("\"{k}\": \""))? + k.len() + 5; Some(text[at..].split('"').next()?.to_owned()) };
            let what: Vec<String> = ["model", "operation", "file_path", "command"].iter().filter_map(|k| field(k)).filter(|v| !v.is_empty() && !name.contains(v.as_str())).take(2).collect();
            let what = if what.is_empty() { String::new() } else { format!(" ({})", clip(&what.join(", "), 100)) };
            let going = if it["status"] == "running" || it["status"] == "inProgress" { ", still running" } else { "" };
            format!("used{failed} {}{what}{going}", clip(&name, 140))
        }
        _ => return None,
    };
    Some(format!("{when} {line}"))
}

/// Absolute paths of image files named in `texts` (most relevant first) that are real pictures, without repeats.
pub fn pictures_in<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    const EXT: [&str; 7] = [".png", ".jpg", ".jpeg", ".webp", ".gif", ".heic", ".tiff"];
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        let lower = text.to_ascii_lowercase();
        let mut found: Vec<(usize, String)> = Vec::new();
        for ext in EXT {
            let mut from = 0;
            while let Some(i) = lower[from..].find(ext) {
                let end = from + i + ext.len();
                from = end;
                // The name must end there ("a.png.txt" and "a.pngx" are not pictures; "a.png." ending a sentence is).
                let mut after = text[end..].chars();
                match (after.next(), after.next()) {
                    (Some(c), _) if c.is_alphanumeric() || "_-/".contains(c) => continue,
                    (Some('.'), Some(c)) if c.is_alphanumeric() => continue,
                    _ => {}
                }
                let start = text[..end].rfind(|c: char| c.is_whitespace() || "\"'`(=,<>[]".contains(c))
                    .map_or(0, |k| k + text[k..].chars().next().map_or(1, char::len_utf8));
                let tok = &text[start..end];
                let path = if tok.starts_with('/') { tok.to_owned() } else if let Some(rest) = tok.strip_prefix("~/") { format!("{home}/{rest}") } else { continue };
                found.push((start, path));
            }
        }
        // Inside one text the last named file is the newest.
        found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
        for (_, p) in found {
            if out.len() >= PICTURES_CAP { return out }
            if !out.contains(&p) && crate::islands::picture(&p) { out.push(p) }
        }
    }
    out
}

/// The thread as the model reads it, within `THREAD_BUDGET` (the oldest activity goes first when it is long).
pub fn render_thread(raw: &ThreadRaw, now: chrono::DateTime<chrono::Utc>) -> ThreadInput {
    let t = &raw.read["thread"];
    let status = t["status"].as_str().unwrap_or("unknown");
    let pending = t["pendingRequestCount"].as_u64().unwrap_or(0);
    let items: Vec<&Value> = raw.read["items"].as_array().into_iter().flatten().collect();
    let read_of = |k: &Value| raw.child_reads.iter().find(|(c, _)| Some(c.as_str()) == k["threadId"].as_str()).map(|(_, r)| r);
    let kids: Vec<(&Value, bool)> = raw.children.iter().map(|k| (k, child_working(k, read_of(k), now))).collect();
    let latest_run = raw.read["thread"]["latestRunId"].as_str();
    let ended = !matches!(status, "preparing" | "queued" | "starting" | "running" | "waiting");
    // The turn ended normally within the refresher's window (55 min: the binding settles at 60, so the two never
    // disagree at the edge). Only then do sub-agents and background work make a finished turn "working"; a failed,
    // stopped or long-finished thread is the binding's.
    let ended_recently = matches!(status, "completed" | "idle") && raw.read["recentRuns"].as_array().and_then(|r| r.first())
        .and_then(|run| run["completedAt"].as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| (now - t.with_timezone(&chrono::Utc)).num_minutes() < 55);
    let finished_turn = matches!(status, "completed" | "idle");
    // Background commands its latest run left going (they outlive the turn; their end starts a new run).
    let background: Vec<&Value> = if ended_recently { background_commands(&items, latest_run, now) } else { Vec::new() };
    let working = !ended || pending > 0 || finished_turn && kids.iter().any(|(_, w)| *w) || !background.is_empty();
    let ago = |ts: &Value| -> String {
        let Some(t) = ts.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) else { return String::new() };
        let m = (now - t.with_timezone(&chrono::Utc)).num_minutes();
        if m < 1 { "just now".into() } else if m < 120 { format!("{m} min ago") } else { format!("{} h ago", m / 60) }
    };

    // The job is one entity: the thread, its sub-agents and the background work it started. What moves now first.
    let mut head = vec!["Right now (one job: this thread, its sub-agents and its background work; newest first):".to_string()];
    let mut live: Vec<(String, String)> = Vec::new();
    for (k, _) in kids.iter().filter(|(_, w)| *w) {
        let title = clip(without_tag(k["title"].as_str().unwrap_or("sub-agent")), 60);
        let last = read_of(k).and_then(|r| r["items"].as_array().into_iter().flatten().rev().find_map(|it| activity_line(it).map(|l| (l, it["updatedAt"].clone()))));
        let line = match last {
            Some((l, ts)) => format!("- sub-agent \"{title}\": working; latest step ({}): {}", ago(&ts), clip(&l, 180)),
            None => format!("- sub-agent \"{title}\": working; last change {} ({})", local_hhmm(&k["updatedAt"]), ago(&k["updatedAt"])),
        };
        live.push((k["updatedAt"].as_str().unwrap_or("").to_owned(), line));
    }
    for it in &background {
        let cmd = it["text"].as_str().unwrap_or("").lines().next().unwrap_or("").trim_start_matches("$ ");
        live.push((it["updatedAt"].as_str().unwrap_or("").to_owned(),
            format!("- background command, started {} ({}): {}", local_hhmm(&it["updatedAt"]), ago(&it["updatedAt"]), clip(cmd, 140))));
    }
    live.sort_by(|a, b| b.0.cmp(&a.0));
    head.extend(live.into_iter().map(|(_, l)| l));
    let mut own = format!("- the thread itself \"{}\": {status}", without_tag(t["title"].as_str().unwrap_or("")));
    if pending > 0 { own.push_str(&format!(", {pending} question(s) waiting for User's answer")); }
    if let Some(run) = raw.read["recentRuns"].as_array().and_then(|r| r.first()) {
        if !run["completedAt"].is_null() && ended { own.push_str(&format!("; its own turn ended {} ({})", local_hhmm(&run["completedAt"]), ago(&run["completedAt"]))); }
    }
    if let Some(m) = items.iter().rev().find(|it| it["type"] == "assistant_message") {
        own.push_str(&format!("; its last message {} ({}): \"{}\"", local_hhmm(&m["updatedAt"]), ago(&m["updatedAt"]), clip(&one_line(m["text"].as_str().unwrap_or("")), 200)));
    }
    head.push(own);
    let done: Vec<String> = kids.iter().filter(|(_, w)| !*w).take(8)
        .map(|(k, _)| format!("\"{}\" ({})", clip(without_tag(k["title"].as_str().unwrap_or("sub-agent")), 60), local_hhmm(&k["updatedAt"]))).collect();
    if !done.is_empty() { head.push(format!("Finished sub-agents: {}", done.join(", "))); }
    // Pictures: the thread's own newest first, then its sub-agents'.
    let texts = |items: &[&Value]| -> Vec<String> {
        items.iter().rev().map(|it| format!("{} {}", it["title"].as_str().unwrap_or(""), it["text"].as_str().unwrap_or(""))).collect()
    };
    let mut all_texts = texts(&items);
    for (_, r) in &raw.child_reads {
        let ci: Vec<&Value> = r["items"].as_array().into_iter().flatten().collect();
        all_texts.extend(texts(&ci));
    }
    let pictures = pictures_in(all_texts.iter().map(String::as_str));
    let mut lines = head;
    if !pictures.is_empty() {
        lines.push("Pictures it and its sub-agents produced (its own newest first, then its sub-agents'):".to_string());
        for p in &pictures { lines.push(format!("- {p}")); }
    }
    // The header and pictures always stay (at most about 3k characters); the oldest activity goes first.
    let mut activity: Vec<String> = items.iter().filter_map(|it| activity_line(it)).map(|l| format!("  {l}")).collect();
    activity.dedup();
    let fixed: usize = lines.iter().map(|l| l.len() + 1).sum::<usize>() + 32;
    let mut used: usize = activity.iter().map(|l| l.len() + 1).sum();
    while fixed + used > THREAD_BUDGET && !activity.is_empty() { used -= activity.remove(0).len() + 1; }
    lines.push("Recent activity, oldest first:".into());
    lines.extend(activity);

    let mut h = DefaultHasher::new();
    (status, pending, t["itemCount"].as_u64()).hash(&mut h);
    for (k, w) in &kids { (k["threadId"].as_str(), k["itemCount"].as_u64(), *w).hash(&mut h); }
    ThreadInput { text: lines.join("\n"), pictures, working, stamp: h.finish(), status: status.to_owned(), asking: status == "waiting" || pending > 0, listed: raw.listed, ended_recently }
}

/// At most `n` characters, with an ellipsis when cut.
pub fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_owned() }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    //! Moved from the plan refresher's tests with `render_thread` (30 Oct).
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn png(dir: &Path, name: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
        p.to_string_lossy().into_owned()
    }

    fn thread_raw(status: &str, items: u64, children: Value) -> ThreadRaw {
        ThreadRaw {
            read: json!({"thread": {"threadId": "t-site", "title": "Arsenal website rebuild [heimdall:ec97]", "status": status,
                                     "pendingRequestCount": 0, "itemCount": items, "projectId": "p"},
                         "recentRuns": [{"status": "completed", "completedAt": "2026-10-22T12:03:07Z"}],
                         "items": []}),
            children: children.as_array().cloned().unwrap_or_default(),
            child_reads: vec![],
            listed: true,
        }
    }

    #[test]
    fn a_thread_is_read_as_activity_sub_agents_and_pictures() {
        let d = tempfile::tempdir().unwrap();
        let shot = png(d.path(), "seek.png");
        let now = chrono::Utc::now();
        let fresh = (now - chrono::Duration::seconds(60)).to_rfc3339();
        let mut raw = thread_raw("completed", 1751, json!([
            {"threadId": "c1", "parentThreadId": "t-site", "title": "Colour test", "status": "idle", "settled": false, "updatedAt": fresh, "itemCount": 117},
            {"threadId": "c2", "parentThreadId": "t-site", "title": "Research audio models", "status": "idle", "settled": false, "updatedAt": "2026-10-22T11:44:50Z", "itemCount": 95}]));
        raw.read["items"] = json!([
            {"type": "user_message", "text": "Rebuild the Arsenal website", "updatedAt": "2026-10-22T11:50:00Z"},
            {"type": "reasoning", "text": "secret thoughts", "updatedAt": "2026-10-22T11:51:00Z"},
            {"type": "file_change", "text": format!("{shot}\n{{\"type\":\"create\"}}"), "updatedAt": "2026-10-22T11:56:52Z"},
            {"type": "command_execution", "status": "failed", "text": "$ node seek-check.mjs\nError", "updatedAt": "2026-10-22T11:58:13Z"},
            {"type": "dynamic_tool", "title": null, "text": "{\n  \"toolName\": \"mcp__arsenal__arsenal_run\",\n  \"input\"", "updatedAt": "2026-10-22T11:59:00Z"},
            {"type": "assistant_message", "text": "I'm waiting on the three test workers.", "updatedAt": "2026-10-22T12:03:07Z"},
            {"type": "checkpoint", "text": "[]", "updatedAt": "2026-10-22T12:03:07Z"}]);
        raw.child_reads.push(("c1".into(), json!({"items": [{"type": "assistant_message", "text": "DDColor frames done", "updatedAt": fresh}]})));
        let t = render_thread(&raw, now);
        assert!(t.working, "its own turn ended, a sub-agent still works");
        // One job, what moves now first: the working sub-agent's latest step, then the thread's own (older) last word.
        assert!(t.text.starts_with("Right now (one job: this thread, its sub-agents and its background work; newest first):\n- sub-agent \"Colour test\": working; latest step (1 min ago): "), "{}", t.text);
        assert!(t.text.contains("said: DDColor frames done"));
        assert!(t.text.contains("- the thread itself \"Arsenal website rebuild\": completed") && t.text.contains("its last message") && t.text.contains("\"I'm waiting on the three test workers.\""));
        assert!(t.text.contains("Finished sub-agents: \"Research audio models\""));
        for want in ["asked: Rebuild the Arsenal website", &format!("wrote {shot}"), "ran (failed): node seek-check.mjs", "used mcp__arsenal__arsenal_run", "said: I'm waiting on the three test workers."] {
            assert!(t.text.contains(want), "missing {want:?} in\n{}", t.text);
        }
        assert!(!t.text.contains("secret thoughts"), "thinking is not activity");
        assert_eq!(t.pictures, vec![shot.clone()]);
        assert!(t.text.contains(&format!("(its own newest first, then its sub-agents'):\n- {shot}\nRecent activity")));
        assert_eq!(t.status, "completed");
        // The stamp moves with the thread and its sub-agents, not with the clock.
        assert_eq!(render_thread(&raw, now + chrono::Duration::seconds(5)).stamp, t.stamp);
        let mut moved = raw.clone();
        moved.children[0]["itemCount"] = json!(118);
        assert_ne!(render_thread(&moved, now).stamp, t.stamp);
        // Quiet sub-agents whose last word was their report, and a completed turn: finished.
        let quiet = render_thread(&raw, now + chrono::Duration::minutes(10));
        assert!(!quiet.working);
        // Background work the latest run left going is the job working, and it is listed under Right now.
        let mut bg = raw.clone();
        bg.children.clear();
        bg.child_reads.clear();
        bg.read["thread"]["latestRunId"] = json!("run-12");
        bg.read["items"].as_array_mut().unwrap().push(json!({"type": "command_execution", "runId": "run-12", "updatedAt": fresh,
            "text": "$ blender -b b29.blend -a\nCommand running in background with ID: b7"}));
        let mut tool = raw.clone();
        tool.child_reads[0].1["items"] = json!([{"type": "dynamic_tool", "status": "running", "title": "Arsenal Run", "updatedAt": fresh,
            "text": "{\n  \"toolName\": \"mcp__arsenal__arsenal_run\",\n  \"input\": {\n    \"model\": \"seedvr2-3b\",\n    \"input\": {"}]);
        assert!(render_thread(&tool, now).text.contains("used Arsenal Run (seedvr2-3b), still running"));
        bg.read["recentRuns"][0]["completedAt"] = json!(fresh);
        let b = render_thread(&bg, now);
        assert!(b.working && b.text.contains("- background command, started ") && b.text.contains("blender -b b29.blend -a"), "{}", b.text);
        let mut ended_bg = bg.clone();
        ended_bg.read["items"].as_array_mut().unwrap().push(json!({"type": "user_message", "runId": "run-12", "updatedAt": fresh, "text": "1 background task finished"}));
        assert!(!render_thread(&ended_bg, now).working, "a background task that reported finished");
        let mut grep = bg.clone();
        grep.read["items"].as_array_mut().unwrap().last_mut().unwrap()["text"] = json!("$ grep 'Command running in background with ID' log\n(no match)");
        assert!(!render_thread(&grep, now).working, "the phrase in the command line is not the tool's notice");
        let mut stopped = bg.clone();
        stopped.read["thread"]["status"] = json!("interrupted");
        assert!(!render_thread(&stopped, now).working, "a stopped thread is the binding's (failed), whatever runs");
        bg.read["items"].as_array_mut().unwrap().last_mut().unwrap()["runId"] = json!("run-11");
        assert!(!render_thread(&bg, now).working, "an earlier run's background command has had its own ending");
        // Quiet for ten minutes but its latest step is a command it waits on (a GPU lock, a render): still working.
        let mut waiting = raw.clone();
        waiting.child_reads[0].1["items"] = json!([{"type": "command_execution", "text": "$ wait-for-gpu-lock && render", "updatedAt": fresh},
                                                   {"type": "reasoning", "text": "waiting", "updatedAt": fresh}]);
        assert!(render_thread(&waiting, now + chrono::Duration::minutes(10)).working);
        assert!(!render_thread(&waiting, now + chrono::Duration::minutes(70)).working, "not after an hour of silence");
        // A long thread keeps its newest activity and its pictures.
        let mut long = raw.clone();
        long.read["items"] = Value::Array((0..400).map(|i| json!({"type": "assistant_message", "text": format!("step {i} {}", "x".repeat(60)), "updatedAt": "2026-10-22T12:00:00Z"}))
            .chain(raw.read["items"].as_array().unwrap().iter().cloned()).collect());
        let lt = render_thread(&long, now);
        assert!(lt.text.chars().count() <= THREAD_BUDGET && lt.text.contains("waiting on the three test workers") && !lt.text.contains("step 0 "));
        assert!(lt.text.contains(&format!("- {shot}")), "the pictures survive a long thread");
    }

    #[test]
    fn pictures_are_found_in_commands_reads_and_json_only_when_real() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (png(d.path(), "a.png"), png(d.path(), "shot b.JPG"));
        let fake = d.path().join("fake.png");
        std::fs::write(&fake, b"text").unwrap();
        let texts = [
            format!("Read {a}"),
            format!("$ ffmpeg -i {a} -vf crop /tmp/none.png && open \"{b}\""),
            format!("{{\"file_path\": \"{}\"}} {a}.txt {a}x", fake.display()),
        ];
        let got = pictures_in(texts.iter().map(String::as_str));
        assert_eq!(got, vec![a.clone()], "the spaced name is cut at the space; a fake, a missing file and longer names are not pictures");
        let spaced = format!("open '{b}'");
        assert!(pictures_in(std::iter::once(spaced.as_str())).is_empty(), "a path with a space is not guessed");
    }
}
