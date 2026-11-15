//! Scheduled items for wakes that carry out the user's own request (bug note 2026-11-14T16-58-00). The user, 16:57: "I
//! don't understand why is it not scheduled? Should be scheduled then." Heimdall had queued the work he asked for
//! only as its own `schedule_wake`, which he never sees.
//!
//! A `schedule_wake` with `user_asked` (their words) adds one Scheduled item from the same call: the title (`item`, or
//! the note's first line), the wake's time as `due`, their words as the yes, and `wake: <id>` linking the two. When the
//! wake is delivered the item is ticked; when it is cancelled the item goes. A wake without `user_asked` adds nothing.
//! The item's own "due" event says nothing to the agent (`responses.rs`): the wake brings the work.

use super::{files::Root, plan};
use anyhow::Result;
use serde_json::{json, Map, Value};

/// Longest title taken from the note when the call gives no `item`.
const TITLE_CHARS: usize = 64;

/// The title for the item: `item`, else the note's first line, cut at a word to `TITLE_CHARS`.
pub fn title(item: Option<&str>, message: &str) -> String {
    if let Some(t) = item.map(str::trim).filter(|t| !t.is_empty()) {
        return t.chars().take(100).collect();
    }
    let first = message.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("Scheduled work");
    if first.chars().count() <= TITLE_CHARS { return first.to_owned() }
    let cut: String = first.chars().take(TITLE_CHARS - 1).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end_matches([',', ';', ':', '-', ' ']);
    format!("{cut}…")
}

/// Add the Scheduled item for wake `wake` (due at `due_s`, Unix seconds) once: a replayed call finds it by the link
/// and adds nothing. Returns the line for the agent.
pub fn add(root: &Root, wake: &str, due_s: i64, title: &str, quote: &str, now: i64) -> Result<String> {
    let quote = crate::goals::ops::cap(quote.trim(), 200);
    plan::edit_list(root, "scheduled", |its| {
        if let Some(i) = its.iter().find(|i| i.get("wake").and_then(Value::as_str) == Some(wake)) {
            return Ok(format!("It is in {}'s Scheduled list as '{}'.", super::user(), i.get("title").and_then(Value::as_str).unwrap_or(title)));
        }
        let mut new = Map::new();
        new.insert("id".into(), json!(plan::unique_id(title, its)));
        new.insert("title".into(), json!(title));
        new.insert("done".into(), json!(false));
        new.insert("due".into(), json!(due_s));
        new.insert("yes".into(), json!({"by": "voice", "at": now, "quote": quote}));
        new.insert("wake".into(), json!(wake));
        its.push(new);
        Ok(format!("It shows in {}'s Scheduled list as '{title}'.", super::user()))
    })
}

/// The wake left pending for good: `delivered` ticks its item, `cancelled` takes it off. Returns whether an item changed.
pub fn settle(root: &Root, wake: &str, state: &str) -> Result<bool> {
    let found = root.read("scheduled").map(|o| plan::items(&o)).unwrap_or_default().into_iter()
        .find(|i| i.get("wake").and_then(Value::as_str) == Some(wake));
    let Some(item) = found else { return Ok(false) };
    let id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    match state {
        "delivered" | "sent" => {
            if item.get("done").and_then(Value::as_bool) == Some(true) { return Ok(false) }
            Ok(plan::tick(root, "scheduled", &id, true)?.is_some())
        }
        "cancelled" => plan::edit_list(root, "scheduled", |its| {
            let before = its.len();
            its.retain(|i| i.get("id").and_then(Value::as_str) != Some(id.as_str()));
            Ok(its.len() != before)
        }),
        _ => Ok(false),
    }
}

/// Wakes settled before the islands host was up (a late delivery at startup): settled by `drain` at `install`
/// (night 1115 review S3), so the item does not sit open and overdue until the next wake tool call.
static EARLY: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Remember a settle that came before the host (see `EARLY`).
pub fn queue_early(wake: &str, state: &str) {
    EARLY.lock().unwrap_or_else(|e| e.into_inner()).push((wake.to_owned(), state.to_owned()));
}

/// Settle what `queue_early` kept; returns how many items changed.
pub fn drain(root: &Root) -> usize {
    let early: Vec<_> = std::mem::take(&mut *EARLY.lock().unwrap_or_else(|e| e.into_inner()));
    early.into_iter().filter(|(w, st)| settle(root, w, st).unwrap_or_else(|e| { eprintln!("Scheduled item for wake {w}: {}", super::short_error(&e)); false })).count()
}

/// Settle every open linked item whose wake is no longer pending (`state` reads the wake store; `None`: no such wake,
/// treated as cancelled). Catches up on a delivery or cancel that came while the islands host was not running.
pub fn reconcile(root: &Root, state: impl Fn(&str) -> Option<String>) -> Result<usize> {
    let open: Vec<String> = root.read("scheduled").map(|o| plan::items(&o)).unwrap_or_default().into_iter()
        .filter(|i| i.get("done").and_then(Value::as_bool) != Some(true))
        .filter_map(|i| i.get("wake").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut changed = 0;
    for wake in open {
        let s = state(&wake).unwrap_or_else(|| "cancelled".into());
        if s != "pending" && settle(root, &wake, &s)? { changed += 1 }
    }
    Ok(changed)
}

/// Whether a Scheduled item is a wake's (its due time is the wake's, which tells the agent itself).
pub fn is_wake_item(root: &Root, item_id: &str) -> bool {
    root.read("scheduled").map(|o| plan::items(&o)).unwrap_or_default().into_iter()
        .any(|i| i.get("id").and_then(Value::as_str) == Some(item_id) && i.get("wake").and_then(Value::as_str).is_some_and(|w| !w.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, Root) {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        (d, r)
    }
    fn scheduled(r: &Root) -> Vec<Map<String, Value>> { r.read("scheduled").map(|o| plan::items(&o)).unwrap_or_default() }

    #[test]
    fn a_wake_for_his_request_shows_in_scheduled_with_his_words_and_time_once() {
        let (_d, r) = root();
        let t = title(None, "Run the Fluid Speech Pro test while the Mac is free\nthen report");
        assert_eq!(t, "Run the Fluid Speech Pro test while the Mac is free");
        add(&r, "wake-e04d08", 1_794_000_000, &t, "for the night when there is free time", 1_793_990_000).unwrap();
        add(&r, "wake-e04d08", 1_794_000_000, &t, "for the night when there is free time", 1_793_990_000).unwrap();
        let items = scheduled(&r);
        assert_eq!(items.len(), 1, "a replay adds nothing: {items:?}");
        assert_eq!(items[0]["title"], t);
        assert_eq!(items[0]["due"], 1_794_000_000);
        assert_eq!(items[0]["wake"], "wake-e04d08");
        assert_eq!(items[0]["yes"]["quote"], "for the night when there is free time");
        assert!(is_wake_item(&r, items[0]["id"].as_str().unwrap()));
    }

    #[test]
    fn the_item_is_ticked_when_the_wake_is_delivered_and_goes_when_it_is_cancelled() {
        let (_d, r) = root();
        add(&r, "wake-aaaaaa", 1_794_000_000, "Decider: v7 run", "run v7 tonight", 0).unwrap();
        add(&r, "wake-bbbbbb", 1_794_000_100, "Kokoro: test", "try Kokoro later", 0).unwrap();
        assert!(settle(&r, "wake-aaaaaa", "delivered").unwrap());
        assert!(!settle(&r, "wake-aaaaaa", "delivered").unwrap(), "once");
        assert!(settle(&r, "wake-bbbbbb", "cancelled").unwrap());
        let items = scheduled(&r);
        assert_eq!(items.len(), 1);
        assert_eq!((items[0]["wake"].as_str(), items[0]["done"].as_bool()), (Some("wake-aaaaaa"), Some(true)));
        // A delivery missed while the host was down is caught up from the wake store.
        add(&r, "wake-cccccc", 1_794_000_200, "Vella: re-measure", "re-measure Vella", 0).unwrap();
        add(&r, "wake-dddddd", 1_794_000_300, "Klein: speed", "Klein speed later", 0).unwrap();
        let n = reconcile(&r, |w| Some(if w == "wake-cccccc" { "delivered" } else { "pending" }.into())).unwrap();
        assert_eq!(n, 1);
        let done: Vec<_> = scheduled(&r).into_iter().filter(|i| i["done"] == true).map(|i| i["wake"].as_str().unwrap().to_owned()).collect();
        assert_eq!(done, ["wake-aaaaaa", "wake-cccccc"]);
    }

    #[test]
    fn a_plain_item_is_not_a_wake_s_and_long_notes_make_short_titles() {
        let (_d, r) = root();
        assert_eq!(settle(&r, "wake-none", "cancelled").unwrap(), false);
        assert_eq!(reconcile(&r, |_| None).unwrap(), 0);
        assert!(scheduled(&r).is_empty());
        let t = title(None, &"word ".repeat(40));
        assert!(t.chars().count() <= TITLE_CHARS && t.ends_with('…'), "{t}");
        assert_eq!(title(Some("  Arsenal: push  "), "x"), "Arsenal: push");
    }
}
