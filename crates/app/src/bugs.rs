//! Bug notes: Heimdall writes them itself with its `bug_note` MCP tool, in plain English; nothing
//! The user says starts one. They go to the one inbox the night fixing round reads (`heimdall_night::bug_notes`), the
//! menu shows "Bug Notes (N)" with the list, and the night round resolves handled notes out of the inbox.
//!
//! Automatic command reviews (unsure or failed command decisions) are not bug notes: they go to
//! `bugs/command-reviews` beside the inbox.

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The pipeline's view of the inbox: what the menu lists.
pub struct BugNotes {
    pub inbox: PathBuf,
    /// The Bug Notes window's cards: open notes newest first, then the recently resolved ones (`list`).
    pub listed: Vec<ui::BugNote>,
}

impl BugNotes {
    pub fn new(inbox: PathBuf) -> Self {
        let listed = if crate::edition::PUBLIC { vec![] } else { list(&inbox) };
        Self { inbox, listed }
    }

    /// The notes still in the inbox: the menu's count.
    pub fn open(&self) -> usize {
        self.listed.iter().filter(|n| n.is_open()).count()
    }

    /// Re-read the inbox (the night round or the tool may have changed it). True when the list changed.
    pub fn refresh(&mut self) -> bool {
        // The public edition has no bug notes (decision 18): it never reads an inbox (VM finding P-08).
        if crate::edition::PUBLIC {
            return false;
        }
        let listed = list(&self.inbox);
        if listed == self.listed {
            return false;
        }
        self.listed = listed;
        true
    }

    /// Automatic training examples are kept apart from bug notes and never counted in the menu.
    pub fn write_auto_command(&self, decision: &Value) -> Result<PathBuf> {
        let dir = reviews_dir(&self.inbox);
        heimdall_archive::private_dir(&dir)?;
        let ms = agent::now_ms();
        let base = heimdall_night::bug_notes::local_stamp(ms / 1000, false);
        let mut id = base.clone();
        let mut n = 2;
        while dir.join(format!("{id}.json")).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        let record = json!({"schema":1,"kind":"command_decision_auto","auto":true,
            "id":id,"created_unix_ms":ms,"created_local":heimdall_night::bug_notes::local_stamp(ms/1000,true),
            "note":{"text":format!("Automatic command review: {}", decision["selection"].as_str().or(decision["intent"].as_str()).unwrap_or("decision error"))},
            "command_decision":decision});
        let md = format!("# Automatic command review\n\n{}\n\n```json\n{}\n```\n", record["note"]["text"].as_str().unwrap_or_default(),
            serde_json::to_string_pretty(decision).unwrap_or_default());
        heimdall_archive::atomic_write(&dir.join(format!("{id}.md")), md.as_bytes())?;
        let path = dir.join(format!("{id}.json"));
        heimdall_archive::atomic_write(&path, &serde_json::to_vec_pretty(&record)?)?;
        if let Err(e) = prune_reviews(&dir, REVIEWS_KEPT) {
            eprintln!("Old command reviews not pruned: {e:#}");
        }
        Ok(path)
    }
}

/// Automatic command reviews kept, newest first (review 19 Oct: the folder grew without bound).
pub const REVIEWS_KEPT: usize = 200;
/// Keeps the newest `keep` reviews (each a `.json` record and its `.md`), by their stamp ids (`2026-10-19T12-15-52`,
/// then `-2`, `-3` … within one second).
pub fn prune_reviews(dir: &Path, keep: usize) -> Result<()> {
    let mut ids: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).map(str::to_owned))
        .collect();
    let order = |id: &String| match (id.get(..19), id.get(20..)) {
        (Some(stamp), Some(n)) => (stamp.to_owned(), n.parse::<u64>().unwrap_or(0)),
        _ => (id.clone(), 1),
    };
    ids.sort_by_key(order);
    for id in &ids[..ids.len().saturating_sub(keep)] {
        for ext in ["json", "md"] {
            match std::fs::remove_file(dir.join(format!("{id}.{ext}"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    Ok(())
}

/// `<data>/bugs/inbox`; the personal app uses the code checkout's inbox (where the night round works) when the
/// `bug_inbox` setting names one, else Heimdall's checkout in the home folder when it is there.
/// `HEIMDALL_BUG_INBOX` overrides both.
pub fn inbox_dir(data: &Path, replay: bool) -> PathBuf {
    if let Some(dir) = std::env::var_os("HEIMDALL_BUG_INBOX") {
        return PathBuf::from(dir);
    }
    // Unit tests never write to a configured inbox, whatever their replay flag.
    if !replay && !cfg!(test) && !crate::edition::PUBLIC {
        let checkout = || heimdall_night::bug_notes::checkout_inbox(&heimdall_home::home_dir());
        if let Some(inbox) = heimdall_night::bug_notes::setting_inbox(data).or_else(checkout) {
            return inbox;
        }
    }
    heimdall_night::bug_notes::data_inbox(data)
}

/// `bugs/command-reviews` beside the inbox.
pub fn reviews_dir(inbox: &Path) -> PathBuf {
    inbox.parent().map_or_else(|| inbox.join("command-reviews"), |p| p.join("command-reviews"))
}

/// The spoken-note recorder kept crash checkpoints in `<data>/bugs/pending`. It is gone; remove that folder (and an
/// empty `<data>/bugs`) when empty, so nothing looks like a second inbox. A non-empty one is left for a person.
// The public edition never files a note (no `bug_note` tool, docs/bugs.md).
#[cfg_attr(feature = "public", allow(dead_code))]
pub fn retire_spoken_note_checkpoints(data: &Path) {
    let pending = data.join("bugs/pending");
    if std::fs::remove_dir(&pending).is_err() && pending.is_dir() {
        eprintln!("Old spoken bug-note checkpoints left in {}; the recorder is gone, nothing reads them", pending.display());
        return;
    }
    let _ = std::fs::remove_dir(data.join("bugs"));
}

/// Resolved notes stay in the list, as fixed or closed cards, this long after they were resolved.
pub const RESOLVED_SHOWN_SECS: u64 = 3 * 86_400;

/// The window's cards: the inbox's notes (newest first, at most 500), then the notes resolved in the last three days
/// (latest first, at most 30), so a fix shows with its commit.
pub fn list(inbox: &Path) -> Vec<ui::BugNote> {
    let now = now_unix();
    let mut out: Vec<ui::BugNote> = heimdall_night::bug_notes::list(inbox).iter().take(500).map(|r| list_item(r, now)).collect();
    // A note being resolved can be in both folders for a moment: listed once, as open.
    let resolved: Vec<ui::BugNote> = recently_resolved(inbox, now).iter().take(30).map(|r| list_item(r, now)).filter(|n| !out.iter().any(|o| o.id == n.id)).collect();
    out.extend(resolved);
    out
}

/// Notes resolved within `RESOLVED_SHOWN_SECS` of `now`, latest first. Only files written in that time are read (a
/// resolution writes its record).
fn recently_resolved(inbox: &Path, now: u64) -> Vec<Value> {
    let Ok(dir) = heimdall_night::bug_notes::resolved_dir(inbox) else { return vec![] };
    let since = now.saturating_sub(RESOLVED_SHOWN_SECS);
    let recent = |p: &Path| {
        std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).is_some_and(|d| d.as_secs() >= since)
    };
    let mut out: Vec<Value> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json") && recent(p))
        .filter_map(|p| serde_json::from_slice::<Value>(&std::fs::read(&p).ok()?).ok())
        .filter(|v| v["kind"] == heimdall_night::bug_notes::KIND && v["resolution"].is_object())
        .filter(|v| v["resolution"]["resolved_unix_ms"].as_u64().is_some_and(|ms| ms / 1000 >= since))
        .collect();
    out.sort_by_key(|v| std::cmp::Reverse(v["resolution"]["resolved_unix_ms"].as_u64()));
    out
}

/// One card from a note's record: its title, cause, files, evidence and status.
pub fn list_item(record: &Value, now: u64) -> ui::BugNote {
    let ms = record["created_unix_ms"].as_u64().unwrap_or(0);
    let text = record["note"]["text"].as_str().unwrap_or_default().trim().to_owned();
    let strings = |v: &Value| v.as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let cause = &record["cause"];
    let cause_evidence = strings(&cause["evidence"]);
    let mut evidence: Vec<String> = record["evidence"].as_str().map(str::trim).filter(|e| !e.is_empty()).map(str::to_owned).into_iter().collect();
    evidence.extend(cause_evidence.iter().cloned());
    ui::BugNote {
        id: record["id"].as_str().unwrap_or_default().into(),
        time: time_label(ms / 1000, now),
        title: ui::bug_title(&text),
        class: cause["class"].as_str().map(str::to_owned),
        cause: cause["reasoning"].as_str().map(str::trim).filter(|c| !c.is_empty()).map(str::to_owned),
        files: ui::bug_files(&strings(&cause["suspected_files"]), &cause_evidence),
        evidence,
        status: match record["resolution"]["text"].as_str() {
            Some(r) => ui::bug_status(r),
            None if record["resolution"].is_object() => ui::BugStatus::Closed,
            None => ui::BugStatus::Open,
        },
        text,
    }
}

/// The `bug_note` tool's side, shared with the MCP host: the pipeline sets the inbox, the UI attach sets the island.
// The public edition never files a note (no `bug_note` tool, docs/bugs.md).
#[cfg_attr(feature = "public", allow(dead_code))]
#[derive(Default)]
pub struct Filing {
    inbox: Mutex<Option<PathBuf>>,
    ui: Mutex<Option<ui::Ui>>,
    /// Whether a filed note starts a bug scout (`heimdall_night::bug_scout`): the installed app's inbox only.
    scout: std::sync::atomic::AtomicBool,
}

// The public edition never files a note (no `bug_note` tool, docs/bugs.md).
#[cfg_attr(feature = "public", allow(dead_code))]
impl Filing {
    pub fn set_inbox(&self, inbox: PathBuf) {
        *self.inbox.lock().unwrap() = Some(inbox);
    }
    pub fn set_scout(&self, on: bool) {
        self.scout.store(on, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn set_ui(&self, ui: ui::Ui) {
        *self.ui.lock().unwrap() = Some(ui);
    }

    /// `bug_note`: write the note (once per operation, so a replayed call never files it twice), start its scout, refresh
    /// the menu, and answer with one short line. The scout writes only the note's own files; nothing here wakes the agent.
    pub fn file(&self, actions: &agent::actions::Actions, args: &Value, op: &str, p: &agent::delivery::DeliveryPermit) -> Result<String> {
        let inbox = self.inbox.lock().unwrap().clone().context("BUG_INBOX_UNAVAILABLE: Heimdall is still starting; try again in a moment")?;
        let receipt = actions.receipt(op, "bug_note", &args.to_string(), None, p)?;
        if actions.state(&receipt)? == "pending" {
            let text = args["text"].as_str().context("text")?;
            let note = p.with(|| heimdall_night::bug_notes::write(&inbox, text, args["evidence"].as_str(), agent::now_ms()))?;
            actions.set_state(&receipt, "saved", p)?;
            if let (true, Some(id)) = (self.scout.load(std::sync::atomic::Ordering::Relaxed), note["id"].as_str()) {
                heimdall_night::bug_scout::enqueue(&inbox, id, crate::background_gate::allowed);
            }
        }
        let listed = list(&inbox);
        if let Some(ui) = *self.ui.lock().unwrap() {
            ui.send(ui::UiUpdate::BugNotes(listed.clone()));
        }
        Ok(format!("Bug note saved; {} open.", listed.iter().filter(|n| n.is_open()).count()))
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn local(secs: u64) -> libc::tm {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// "14:05" today, "Sun 14:05" within the last six days, else "12 Oct 14:05".
pub fn time_label(secs: u64, now: u64) -> String {
    let (t, n) = (local(secs), local(now));
    let clock = format!("{:02}:{:02}", t.tm_hour, t.tm_min);
    let same_day = t.tm_year == n.tm_year && t.tm_yday == n.tm_yday;
    if same_day {
        clock
    } else if now >= secs && now - secs < 6 * 86_400 {
        format!("{} {clock}", ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][t.tm_wday as usize % 7])
    } else {
        const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
        format!("{} {} {clock}", t.tm_mday, MONTHS[t.tm_mon as usize % 12])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permit() -> (std::sync::Arc<agent::delivery::DeliveryGate>, agent::delivery::DeliveryPermit) {
        let gate = std::sync::Arc::new(agent::delivery::DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        (gate, permit)
    }

    #[test]
    fn labels() {
        let now = 1_760_270_000;
        assert_eq!(time_label(now, now).len(), 5);
        assert!(time_label(now - 2 * 86_400, now).len() == 9);
        assert!(time_label(now - 30 * 86_400, now).split(' ').count() == 3);
    }

    #[test]
    fn the_tool_files_one_note_per_call_and_the_menu_counts_the_store() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let actions = agent::actions::Actions::open(&t.path().join("actions.sqlite")).unwrap();
        let (_gate, p) = permit();
        let filing = Filing::default();
        let args = json!({"text":"t3_workers returned a wall of JSON at 12:32; expected one line per worker.","evidence":"{\"workers\":[…]}"});
        assert!(filing.file(&actions, &args, "op-0", &p).unwrap_err().to_string().starts_with("BUG_INBOX_UNAVAILABLE"));
        filing.set_inbox(inbox.clone());
        assert_eq!(filing.file(&actions, &args, "op-1", &p).unwrap(), "Bug note saved; 1 open.");
        // The same operation replayed (a retried tool call) files nothing new.
        assert_eq!(filing.file(&actions, &args, "op-1", &p).unwrap(), "Bug note saved; 1 open.");
        assert_eq!(filing.file(&actions, &json!({"text":"Ślad: ünïcödé ✓"}), "op-2", &p).unwrap(), "Bug note saved; 2 open.");
        let mut notes = BugNotes::new(inbox.clone());
        assert_eq!(notes.listed.len(), 2);
        assert_eq!(notes.listed[0].text, "Ślad: ünïcödé ✓");
        assert_eq!(ui::bug_notes_title(notes.open()), "Bug Notes (2)");
        // The night round fixes one: the menu's next refresh counts one open, and its card shows the fix's commit.
        let fixed = notes.listed[1].id.clone();
        heimdall_night::bug_notes::resolve(&inbox, &fixed, "Fixed by 4b9117a2: one line per worker.", agent::now_ms()).unwrap();
        assert!(notes.refresh());
        assert_eq!(notes.open(), 1);
        assert_eq!(notes.listed.len(), 2);
        assert_eq!(notes.listed[1].id, fixed);
        assert_eq!(notes.listed[1].status, ui::BugStatus::Fixed(Some("4b9117a2".into())));
        assert!(!notes.refresh());
        // Automatic command reviews never count as bug notes.
        notes.write_auto_command(&json!({"intent":"unsure"})).unwrap();
        assert!(!notes.refresh());
        assert_eq!(std::fs::read_dir(t.path().join("bugs/command-reviews")).unwrap().count(), 2);
    }

    /// A scouted note's card: the title without its time, the class, the cause, the files it cites and every piece
    /// of evidence; resolved long ago, it leaves the list.
    #[test]
    fn a_card_carries_the_title_cause_files_evidence_and_status() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let note = heimdall_night::bug_notes::write(&inbox, "At 23:11 the island chat drew its placeholder twice. Expected: one.", Some("screenshot at 23:11"), agent::now_ms()).unwrap();
        let id = note["id"].as_str().unwrap();
        let cause = json!({"class": "code", "reasoning": "The field and a drawn copy both draw it.", "confidence": 0.75, "by": "scout",
            "evidence": ["layout.rs:950 pushes the field", "field.rs:121 sets the placeholder"], "suspected_files": []});
        heimdall_night::bug_notes::set_cause(&inbox, id, &cause).unwrap();
        let cards = list(&inbox);
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(c.title, "The island chat drew its placeholder twice");
        assert_eq!(c.class.as_deref(), Some("code"));
        assert_eq!(c.cause.as_deref(), Some("The field and a drawn copy both draw it."));
        assert_eq!(c.files, vec!["layout.rs:950".to_string(), "field.rs:121".to_string()]);
        assert_eq!(c.evidence.len(), 3);
        assert_eq!(c.status, ui::BugStatus::Open);
        heimdall_night::bug_notes::resolve(&inbox, id, "Not a bug.", agent::now_ms()).unwrap();
        assert_eq!(list(&inbox)[0].status, ui::BugStatus::Closed);
        // Resolved four days ago: gone from the list.
        let old = agent::now_ms() / 1000 + RESOLVED_SHOWN_SECS + 86_400;
        assert!(recently_resolved(&inbox, old).is_empty());
    }

    #[test]
    fn command_reviews_keep_only_the_newest() {
        let t = tempfile::tempdir().unwrap();
        let notes = BugNotes::new(t.path().join("bugs/inbox"));
        let dir = reviews_dir(&notes.inbox);
        // Older seconds, and many within one second (suffixes past -9 sort as numbers).
        std::fs::create_dir_all(&dir).unwrap();
        for id in ["2026-10-18T09-00-00", "2026-10-19T12-00-00", "2026-10-19T12-00-00-2", "2026-10-19T12-00-00-10"] {
            std::fs::write(dir.join(format!("{id}.json")), "{}").unwrap();
            std::fs::write(dir.join(format!("{id}.md")), "x").unwrap();
        }
        std::fs::write(dir.join("README.txt"), "not a review").unwrap();
        prune_reviews(&dir, 2).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["2026-10-19T12-00-00-10.json", "2026-10-19T12-00-00-10.md", "2026-10-19T12-00-00-2.json", "2026-10-19T12-00-00-2.md", "README.txt"]);
        // Writing a review prunes to `REVIEWS_KEPT`.
        for _ in 0..3 {
            notes.write_auto_command(&json!({"intent":"unsure"})).unwrap();
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "json")).count(), 5);
        prune_reviews(&dir, 0).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "only the stranger file stays");
    }

    #[test]
    fn concurrent_tool_calls_each_file_a_note() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("inbox");
        let actions = agent::actions::Actions::open(&t.path().join("actions.sqlite")).unwrap();
        let (_gate, p) = permit();
        let filing = Filing::default();
        filing.set_inbox(inbox.clone());
        let long = "The island froze mid-animation. ".repeat(400);
        std::thread::scope(|s| {
            for i in 0..12 {
                let (filing, actions, p, long) = (&filing, &actions, &p, &long);
                s.spawn(move || filing.file(actions, &json!({"text":format!("{i}: {long}")}), &format!("op-{i}"), p).unwrap());
            }
        });
        let notes = list(&inbox);
        assert_eq!(notes.len(), 12);
        assert!(notes.iter().all(|n| n.text.ends_with("mid-animation.")));
    }

    #[test]
    fn agent_off_files_nothing() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("inbox");
        let actions = agent::actions::Actions::open(&t.path().join("actions.sqlite")).unwrap();
        let (gate, p) = permit();
        let filing = Filing::default();
        filing.set_inbox(inbox.clone());
        gate.store(false, std::sync::atomic::Ordering::Release);
        assert!(filing.file(&actions, &json!({"text":"x"}), "op", &p).is_err());
        assert!(list(&inbox).is_empty());
    }

    #[test]
    fn retiring_the_old_checkpoint_folder_removes_only_empty_folders() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("bugs/pending")).unwrap();
        retire_spoken_note_checkpoints(t.path());
        assert!(!t.path().join("bugs").exists());
        std::fs::create_dir_all(t.path().join("bugs/pending")).unwrap();
        std::fs::write(t.path().join("bugs/pending/x.json"), b"{}").unwrap();
        retire_spoken_note_checkpoints(t.path());
        assert!(t.path().join("bugs/pending/x.json").exists());
        // A replay inbox in the same folder keeps the folder.
        let u = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(u.path().join("bugs/inbox")).unwrap();
        retire_spoken_note_checkpoints(u.path());
        assert!(u.path().join("bugs/inbox").is_dir());
    }
}
