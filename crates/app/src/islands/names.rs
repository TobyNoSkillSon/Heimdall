//! A job island's name, kept per thread (bug note 2026-11-10T18-32-39: the agent renamed 'task-rebuild-the-office' to
//! "Office hardware page", the island retired after the user saw it done, a new message started a new run, and the island
//! came back as "Rebuild the Office"). A title the agent (`islands_put`) or the island agent gave a thread's island is
//! remembered here, beside the islands folder, and a re-made island takes it instead of the placeholder from the task
//! title (`binding::task_island`).
//!
//! Who named an island is its `titleBy`: `task` (code's placeholder from the task title), `agent` (Heimdall's
//! `islands_put`) or `worker` (the island agent). The island agent may rename `task` and `worker` titles, never an
//! `agent` one; the derived title never overwrites either.

use super::files::{read_object, write_atomic, Root};
use serde_json::{json, Map, Value};

pub const BY_TASK: &str = "task";
pub const BY_AGENT: &str = "agent";
pub const BY_WORKER: &str = "worker";

/// Names older than this are forgotten (a thread silent for a month is not coming back).
pub const KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

fn path(root: &Root) -> std::path::PathBuf { root.0.join("titles.json") }

fn now_ms() -> i64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64 }

static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Remembers `title` as the name `by` gave `thread`'s island. Only `agent` and `worker` names are kept.
pub fn remember(root: &Root, thread: &str, title: &str, by: &str) {
    let (thread, title) = (thread.trim(), title.trim());
    if thread.is_empty() || title.is_empty() || !matches!(by, BY_AGENT | BY_WORKER) { return }
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let mut all = read_object(&path(root)).unwrap_or_default();
    let now = now_ms();
    all.retain(|_, v| v["at"].as_i64().is_some_and(|t| now - t < KEEP_MS));
    if all.get(thread).is_some_and(|v| v["title"] == title && v["by"] == by) { return }
    all.insert(thread.to_owned(), json!({"title": title, "by": by, "at": now}));
    if let Err(e) = root.ensure().and_then(|_| write_atomic(&path(root), &Value::Object(all))) {
        eprintln!("islands: title of {thread} not kept: {e:#}");
    }
}

/// The name kept for `thread`'s island, and who gave it.
pub fn named(root: &Root, thread: &str) -> Option<(String, String)> {
    let all: Map<String, Value> = read_object(&path(root))?;
    let v = all.get(thread.trim())?;
    Some((v["title"].as_str()?.to_owned(), v["by"].as_str()?.to_owned()))
}

/// The thread a job island follows (`"thread": {"id": …}` or a plain id).
pub fn thread_of(o: &Map<String, Value>) -> Option<&str> {
    match o.get("thread") {
        Some(Value::Object(t)) => t.get("id").and_then(Value::as_str),
        Some(Value::String(t)) => Some(t.as_str()),
        _ => None,
    }.filter(|t| !t.trim().is_empty())
}

/// Whether the island agent may (re)name this island: its title is code's placeholder or the island agent's own,
/// never one Heimdall gave it. An island older than `titleBy` counts as a placeholder only when its title is what the
/// task title gives (`binding::job_title`, `unique_job_title`'s numbered form) or names nothing.
pub fn worker_may_name(o: &Map<String, Value>, task: &str) -> bool {
    let title = o.get("title").and_then(Value::as_str).unwrap_or("").trim();
    match o.get("titleBy").and_then(Value::as_str) {
        Some(BY_AGENT) => false,
        Some(_) => true,
        None => {
            if crate::island_agent::placeholder_title(title) { return true }
            if task.trim().is_empty() { return false }
            let derived = super::binding::job_title(task);
            let numbered = title.rsplit_once(' ').is_some_and(|(head, n)| n.chars().all(|c| c.is_ascii_digit()) && derived.starts_with(head));
            title.eq_ignore_ascii_case(&derived) || numbered || title.eq_ignore_ascii_case(&old_job_title(task))
        }
    }
}

/// The placeholder rule before 11 Nov (the head before a short colon, else the first three words), so islands made by
/// an older build are still recognized as placeholders.
fn old_job_title(task: &str) -> String {
    let t = task.split(" [heimdall:").next().unwrap_or(task).trim();
    if let Some((head, _)) = t.split_once(':') {
        let n = head.split_whitespace().count();
        if (1..=3).contains(&n) { return head.trim().to_owned() }
    }
    t.split(':').next().unwrap_or(t).split([',', '(', ';']).next().unwrap_or("").split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_kept_per_thread_and_only_the_agents_and_workers() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        remember(&root, "th", "Heimdall bench", BY_TASK);
        assert_eq!(named(&root, "th"), None, "a placeholder is never kept");
        remember(&root, "th", "Office hardware page", BY_AGENT);
        assert_eq!(named(&root, "th"), Some(("Office hardware page".into(), "agent".into())));
        remember(&root, "th", "AI hardware website", BY_WORKER);
        assert_eq!(named(&root, "th").unwrap().0, "AI hardware website");
        assert_eq!(named(&root, "other"), None);
    }

    #[test]
    fn the_worker_names_placeholders_and_its_own_titles_never_the_agents() {
        let o = |v: Value| v.as_object().unwrap().clone();
        let task = "Rebuild the office AI hardware page (Opus 5.5, same brief)";
        assert!(worker_may_name(&o(json!({"title": "Office AI hardware", "titleBy": "task"})), task));
        assert!(worker_may_name(&o(json!({"title": "Hardware page", "titleBy": "worker"})), task));
        assert!(!worker_may_name(&o(json!({"title": "AI hardware website", "titleBy": "agent"})), task));
        // Islands an older build made: the old derived title is a placeholder; anything else is someone's name.
        assert!(worker_may_name(&o(json!({"title": "Rebuild the office"})), task));
        assert!(worker_may_name(&o(json!({"title": "Report"})), "Report: how to build a Heimdall benchmark"));
        assert!(worker_may_name(&o(json!({"title": "Task 2"})), ""));
        assert!(!worker_may_name(&o(json!({"title": "AI hardware website"})), task));
    }
}
