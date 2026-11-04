//! The notch and the floating islands (night 1022, the [notch] part of `docs/archive/briefs/night-1022-islands.md`).
//!
//! - The nudge: when a floating island newly needs the user (its `attention` is set, or a plan island gains an open item
//!   in Waiting on you), the notch plays one small nudge (`ui::UiUpdate::FloatingNudge`), so he notices even when the
//!   side dock is off screen (on Goals, every open ask). `needs` and `nudges` are pure; `listen` subscribes them to the host's
//!   `IslandsEvent::Changed` (this part never watches the folder). The first snapshot after launch is the baseline:
//!   what already needed the user before Heimdall started does not nudge (the islands keep their own attention pulse).
//! - While every island is hidden (⌃⌘,, `visibility.rs`), a result that arrives (an island newly `done` or `failed`)
//!   nudges too, as the bell: the island that shows it is off screen.
//! - Pulling a reply out: `pull_out` keeps the pulled-down reply as markdown in `<root>/docs/<id>.md` and puts a viewer
//!   island for it that grows out of the notch (`origin: {"from": "notch"}`) and stays; `pull_out_turn` (23 Oct) keeps
//!   the turn's notes with it (text and tables in the document, an SVG or an image as its own island).

use super::files::{safe_id, Root};
use super::IslandsEvent;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use ui::agent_notice::Detail;

/// What needs the user now: a key per island with `attention` (its id) and per open waiting item of a plan island
/// (`<island>/<item id>`), with whether it is a question.
pub type Needs = BTreeMap<String, bool>;

/// What needs the user in this snapshot of every island. Hidden islands do not count (nothing on screen to look at).
pub fn needs(islands: &[(String, Map<String, Value>)]) -> Needs {
    let mut out = Needs::new();
    for (id, o) in islands {
        if o.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match o.get("attention").and_then(Value::as_str).map(str::trim) {
            Some("question") => {
                out.insert(id.clone(), true);
            }
            Some("waiting") => {
                out.insert(id.clone(), false);
            }
            _ => {}
        }
        if o.get("preset").and_then(Value::as_str) == Some("todo") {
            // Goals' items are all asks (31 Oct): each open one needs the user; elsewhere, Waiting on you does.
            let asks = o.get("list").and_then(Value::as_str) == Some("goals");
            for item in o.get("items").and_then(Value::as_array).into_iter().flatten() {
                let Some(item) = item.as_object() else { continue };
                let waiting = if asks { item.get("chosen").and_then(Value::as_str).is_none_or(str::is_empty) }
                    else { item.get("group").and_then(Value::as_str) == Some("waiting") };
                let done = item.get("done").and_then(Value::as_bool) == Some(true);
                // The app diffs items by id, falling back to the title.
                let key = item.get("id").and_then(Value::as_str).or_else(|| item.get("title").and_then(Value::as_str));
                if let (true, false, Some(key)) = (waiting, done, key) {
                    out.insert(format!("{id}/{key}"), false);
                }
            }
        }
    }
    out
}

/// One nudge per thing that newly needs the user (`true`: a question), in key order. A waiting island that turns into a
/// question nudges again; one that stays, or goes, does not.
pub fn nudges(before: &Needs, now: &Needs) -> Vec<bool> {
    now.iter().filter(|(k, q)| before.get(*k).is_none_or(|was| !*was && **q)).map(|(_, q)| *q).collect()
}

/// Every island showing a result: id → its state (`done` or `failed`). Hidden islands do not count.
pub type Results = BTreeMap<String, String>;

pub fn results(islands: &[(String, Map<String, Value>)]) -> Results {
    islands
        .iter()
        .filter(|(_, o)| o.get("hidden").and_then(Value::as_bool) != Some(true))
        .filter_map(|(id, o)| o.get("state").and_then(Value::as_str).map(str::trim).filter(|s| matches!(*s, "done" | "failed")).map(|s| (id.clone(), s.to_string())))
        .collect()
}

/// How many results are new: an island that newly shows `done` or `failed`, or changes from one to the other.
pub fn new_results(before: &Results, now: &Results) -> usize {
    now.iter().filter(|(k, s)| before.get(*k) != Some(*s)).count()
}

/// Subscribes to the host's island events: `send(question)` once per island that newly needs the user, and while every
/// island is hidden (`hidden()`) once per new result. Called once at startup; the listener only diffs in memory and
/// hands each nudge to `send` (a `Ui::send`, which never blocks).
pub fn listen(send: impl Fn(bool) + Send + Sync + 'static) {
    listen_with(super::visibility::hidden, send)
}

pub fn listen_with(hidden: fn() -> bool, send: impl Fn(bool) + Send + Sync + 'static) {
    let last: Mutex<Option<(Needs, Results)>> = Mutex::new(None);
    super::subscribe(move |event| {
        let IslandsEvent::Changed(all) = event else { return };
        let now = (needs(all), results(all));
        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((needs_before, results_before)) = last.as_ref() {
            for question in nudges(needs_before, &now.0) {
                send(question);
            }
            if hidden() {
                for _ in 0..new_results(results_before, &now.1) {
                    send(false);
                }
            }
        }
        *last = Some(now);
    });
}

// ---------------------------------------------------------------- pulling a reply out of the notch

/// The viewer island's title: "Reply" and the time it was pulled out, local ("Reply 15:12"). Never the reply's own
/// first words: they are usually a filler ("Right.", "Done."; bug note 22 Oct 15:13, an island titled "Right"), and
/// the island shows the reply itself.
pub fn reply_title(now_ms: u128) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(i64::try_from(now_ms).unwrap_or(i64::MAX)).single() {
        Some(at) => format!("Reply {}", at.format("%H:%M")),
        None => "Reply".into(),
    }
}

/// Keeps `text` (the reply as written, markdown) as `<root>/docs/<id>.md` and puts a viewer island for it that grows
/// out of the notch, open, and stays until the user closes it. `now_ms` names it (`reply-<ms>`). Returns the island id
/// and the document's path.
pub fn pull_out(root: &Root, text: &str, now_ms: u128) -> Result<(String, PathBuf)> {
    let text = text.trim();
    if text.is_empty() {
        bail!("nothing to pull out");
    }
    keep(root, &format!("reply-{now_ms}"), &reply_title(now_ms), "md", format!("{text}\n").as_bytes())
}

/// Writes `bytes` as `<root>/docs/<id>.<ext>` and puts a viewer island titled `title` for it that grows out of the
/// notch, open, and stays until the user closes it.
fn keep(root: &Root, id: &str, title: &str, ext: &str, bytes: &[u8]) -> Result<(String, PathBuf)> {
    let id = safe_id(id);
    let path = root.docs().join(format!("{id}.{ext}"));
    write_atomic(&path, bytes)?;
    let island = json!({
        "preset": "viewer",
        "title": title,
        "path": path.to_string_lossy(),
        "origin": {"from": "notch"},
        "expanded": true,
    });
    let Value::Object(island) = island else { unreachable!("an object literal") };
    root.put(&id, &island)?;
    Ok((id, path))
}

/// A table as markdown (the viewer draws markdown tables); a cell's bars and line breaks are kept readable.
pub fn table_markdown(headers: &[String], rows: &[Vec<String>]) -> String {
    let cell = |c: &str| c.replace('|', "\\|").split_whitespace().collect::<Vec<_>>().join(" ");
    let cols = rows.iter().map(Vec::len).chain([headers.len()]).max().unwrap_or(0).max(1);
    let line = |cells: &[String]| format!("| {} |", (0..cols).map(|i| cell(cells.get(i).map_or("", String::as_str))).collect::<Vec<_>>().join(" | "));
    let mut out = vec![line(headers), format!("|{}|", vec![" --- "; cols].join("|"))];
    out.extend(rows.iter().map(|r| line(r)));
    out.join("\n")
}

/// The image format of `bytes` by its signature (PNG unless it says otherwise).
fn image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.len() > 12 && &bytes[4..8] == b"ftyp" && (&bytes[8..12] == b"heic" || &bytes[8..12] == b"heix" || &bytes[8..12] == b"mif1") {
        "heic"
    } else if bytes.starts_with(b"GIF8") {
        "gif"
    } else {
        "png"
    }
}

/// 23 Oct (the user: "when you're giving me a table inside of it, I cannot actually pull the table out"): the pulled-down
/// turn leaves whole. Its reply (`text`, may be empty) and its text and table notes go into one markdown document,
/// each note under its title in bold; an SVG or an image note becomes its own viewer island (`image` loads an
/// archived image's bytes). A single note pulled out alone is titled by its own title. Returns the islands put.
pub fn pull_out_turn(root: &Root, text: &str, notes: &[(String, Detail)], now_ms: u128, image: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Vec<(String, PathBuf)>> {
    let text = text.trim();
    let title_of = |t: &str| if t.trim().is_empty() { "Note".to_string() } else { t.trim().to_string() };
    let mut doc: Vec<String> = vec![];
    if !text.is_empty() {
        doc.push(text.to_string());
    }
    let mut files: Vec<(String, &'static str, Vec<u8>)> = vec![];
    for (title, detail) in notes {
        let section = |body: String| if text.is_empty() && notes.len() == 1 { body } else { format!("**{}**\n\n{body}", title_of(title)) };
        match detail {
            Detail::Text(t) if !t.trim().is_empty() => doc.push(section(t.trim().to_string())),
            Detail::Table { headers, rows } => doc.push(section(table_markdown(headers, rows))),
            Detail::Svg(svg) => files.push((title_of(title), "svg", svg.as_bytes().to_vec())),
            Detail::Image { archive_id } => match image(archive_id) {
                Some(bytes) => {
                    let ext = image_ext(&bytes);
                    files.push((title_of(title), ext, bytes));
                }
                None => eprintln!("island: the image {archive_id} could not be read to pull it out"),
            },
            Detail::Text(_) => {}
        }
    }
    if doc.is_empty() && files.is_empty() {
        bail!("nothing to pull out");
    }
    let mut out = vec![];
    if !doc.is_empty() {
        let title = if text.is_empty() && notes.len() == 1 { title_of(&notes[0].0) } else { reply_title(now_ms) };
        out.push(keep(root, &format!("reply-{now_ms}"), &title, "md", format!("{}\n", doc.join("\n\n")).as_bytes())?);
    }
    for (k, (title, ext, bytes)) in files.into_iter().enumerate() {
        out.push(keep(root, &format!("note-{now_ms}-{k}"), &title, ext, &bytes)?);
    }
    Ok(out)
}

/// The document through a hidden temp file and rename (the helper may be watching the folder).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("document path has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().and_then(|n| n.to_str()).context("document path has no name")?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn island(id: &str, v: Value) -> (String, Map<String, Value>) {
        let Value::Object(o) = v else { panic!("object") };
        (id.to_string(), o)
    }

    #[test]
    fn attention_that_newly_appears_nudges_once_and_a_question_is_blue() {
        let quiet = vec![island("tests", json!({"title": "Tests", "state": "running"})), island("arsenal", json!({"title": "Arsenal"}))];
        let waiting = vec![island("tests", json!({"title": "Tests", "attention": "waiting"})), island("arsenal", json!({"title": "Arsenal"}))];
        let both = vec![island("tests", json!({"attention": "waiting"})), island("arsenal", json!({"attention": "question"}))];
        assert!(nudges(&needs(&quiet), &needs(&quiet)).is_empty(), "nothing new");
        assert_eq!(nudges(&needs(&quiet), &needs(&waiting)), vec![false]);
        assert!(nudges(&needs(&waiting), &needs(&waiting)).is_empty(), "it stays: no second nudge");
        assert_eq!(nudges(&needs(&waiting), &needs(&both)), vec![true], "only the new one, a question");
        assert!(nudges(&needs(&both), &needs(&quiet)).is_empty(), "cleared: nothing");
        assert_eq!(nudges(&needs(&quiet), &needs(&both)), vec![true, false], "one per island, in id order");
        // Cleared and set again is new again.
        assert_eq!(nudges(&needs(&quiet), &needs(&waiting)), vec![false]);
    }

    #[test]
    fn a_waiting_island_turning_into_a_question_nudges_but_not_the_other_way() {
        let w = vec![island("job", json!({"attention": "waiting"}))];
        let q = vec![island("job", json!({"attention": "question"}))];
        assert_eq!(nudges(&needs(&w), &needs(&q)), vec![true]);
        assert!(nudges(&needs(&q), &needs(&w)).is_empty());
    }

    #[test]
    fn a_plan_islands_new_open_waiting_item_nudges() {
        let plan = |items: Value| vec![island("todo", json!({"preset": "todo", "list": "todo", "title": "To do", "items": items}))];
        let a = plan(json!([{"id": "a", "title": "Arsenal: push branch", "group": "running"}]));
        let b = plan(json!([{"id": "a", "title": "Arsenal: push branch", "group": "running"}, {"id": "b", "title": "Delete old caches", "group": "waiting"}]));
        let b_moved = plan(json!([{"id": "b", "title": "Delete old caches", "group": "waiting"}, {"id": "a", "title": "Arsenal: push branch", "group": "waiting", "done": true}]));
        let c = plan(json!([{"id": "b", "title": "Delete old caches", "group": "waiting"}, {"id": "c", "title": "Approve the menu", "group": "waiting"}]));
        let untitled = plan(json!(["Plain", {"title": "No id yet", "group": "waiting"}]));
        assert_eq!(nudges(&needs(&a), &needs(&b)), vec![false], "a new waiting item");
        assert!(nudges(&needs(&b), &needs(&b_moved)).is_empty(), "reordered, and a ticked one: nothing new");
        assert_eq!(nudges(&needs(&b), &needs(&c)), vec![false], "a second open waiting item is new too");
        assert_eq!(nudges(&needs(&a), &needs(&untitled)), vec![false], "an item without an id counts by its title");
        // Only plan islands' items, and never a hidden island.
        let job = vec![island("job", json!({"preset": "list", "items": [{"id": "x", "group": "waiting"}]}))];
        assert!(needs(&job).is_empty());
        let hidden = vec![island("job", json!({"attention": "question", "hidden": true}))];
        assert!(needs(&hidden).is_empty());
        let odd = vec![island("job", json!({"attention": 3})), island("k", json!({"attention": "loud"}))];
        assert!(needs(&odd).is_empty(), "only the contract's two values");
    }

    #[test]
    fn every_new_open_ask_on_goals_nudges_and_an_answered_one_does_not() {
        let goals = |items: Value| vec![island("goals", json!({"preset": "todo", "list": "goals", "title": "Goals", "goals": [], "items": items}))];
        let none = goals(json!([]));
        let one = goals(json!([{"id": "a", "title": "Pick the cut", "goal": ""}]));
        let later = goals(json!([{"id": "a", "title": "Pick the cut", "chosen": "later"}, {"id": "b", "title": "Test on iPhone", "done": true}]));
        assert_eq!(nudges(&needs(&none), &needs(&one)), vec![false], "an ask needs no group");
        assert!(needs(&later).is_empty(), "put off or done: nothing waits");
    }

    #[test]
    fn a_result_is_new_when_an_island_newly_shows_done_or_failed() {
        let run = vec![island("tests", json!({"state": "running"})), island("site", json!({"state": "done"}))];
        let done = vec![island("tests", json!({"state": "done"})), island("site", json!({"state": "done"}))];
        let failed = vec![island("tests", json!({"state": "failed"})), island("site", json!({"state": "done"}))];
        assert_eq!(new_results(&results(&run), &results(&done)), 1, "tests finished; site was already done");
        assert_eq!(new_results(&results(&done), &results(&done)), 0, "rewritten, nothing new");
        assert_eq!(new_results(&results(&done), &results(&failed)), 1);
        assert_eq!(new_results(&results(&done), &results(&run)), 0, "running again is not a result");
        let fresh = vec![island("new", json!({"state": "done", "value": "412"}))];
        assert_eq!(new_results(&Results::new(), &results(&fresh)), 1, "a new island with a result");
        let hidden = vec![island("new", json!({"state": "done", "hidden": true}))];
        assert!(results(&hidden).is_empty());
    }

    /// With every island hidden, results still reach him on the notch; shown, the island is there.
    #[test]
    fn results_nudge_only_while_every_island_is_hidden() {
        static HIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        listen_with(|| HIDE.load(std::sync::atomic::Ordering::Acquire), move |q| s.lock().unwrap().push(q));
        let e = |state: &str| IslandsEvent::Changed(vec![island("visibility-test-job", json!({"state": state}))]);
        super::super::publish(&e("running"));
        super::super::publish(&e("done"));
        assert!(seen.lock().unwrap().is_empty(), "shown: the island itself shows the result");
        HIDE.store(true, std::sync::atomic::Ordering::Release);
        super::super::publish(&e("running"));
        super::super::publish(&e("failed"));
        assert_eq!(*seen.lock().unwrap(), vec![false], "hidden: one bell for the new result");
        super::super::publish(&e("failed"));
        assert_eq!(seen.lock().unwrap().len(), 1, "the same result again: nothing");
    }

    /// Bug note 22 Oct 15:13: the pulled-out reply "Right. I can fetch a page…" became an island titled "Right".
    /// The title is the time it was pulled out, never the reply's first words.
    #[test]
    fn a_pulled_out_reply_is_titled_by_its_time_not_its_first_word() {
        use chrono::TimeZone;
        let ms = 1_792_674_756_999u128; // 22 Oct 2026, 15:12:36 in Poznań
        let local = chrono::Local.timestamp_millis_opt(ms as i64).single().unwrap();
        assert_eq!(reply_title(ms), format!("Reply {}", local.format("%H:%M")));
        let title = reply_title(ms);
        assert!(title.len() == "Reply 15:12".len() && title[6..].chars().filter(|c| c.is_ascii_digit()).count() == 4, "{title}");
        assert_eq!(reply_title(u128::MAX), "Reply", "an impossible time still has a title");
    }

    #[test]
    fn pulling_out_writes_the_markdown_and_a_viewer_island_from_the_notch() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("FloatingIslands"));
        let text = "**The build is green.**\n\n- Dave pushed the audio fix\n- tests pass\n";
        let (id, path) = pull_out(&root, text, 1_792_596_000_123).unwrap();
        assert_eq!(id, "reply-1792596000123");
        assert_eq!(path, root.docs().join("reply-1792596000123.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "**The build is green.**\n\n- Dave pushed the audio fix\n- tests pass\n");
        let o = root.read(&id).expect("the island");
        assert_eq!(o["id"], json!(id));
        assert_eq!(o["preset"], json!("viewer"));
        assert_eq!(o["title"], json!(reply_title(1_792_596_000_123)));
        assert!(!o["title"].as_str().unwrap().contains("build"), "not the reply's first words");
        assert_eq!(o["path"], json!(path.to_string_lossy()));
        assert_eq!(o["origin"], json!({"from": "notch"}));
        assert_eq!(o["expanded"], json!(true));
        // No temp files left beside them.
        for dir in [root.docs(), root.islands()] {
            let names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
        }
        // Two pulls are two documents; nothing to pull out is refused and writes nothing.
        let (id2, _) = pull_out(&root, "Second.", 1_792_596_000_124).unwrap();
        assert_ne!(id, id2);
        assert!(pull_out(&root, "  \n ", 1_792_596_000_125).is_err());
        assert_eq!(std::fs::read_dir(root.docs()).unwrap().count(), 2);
    }

    /// 23 Oct: a table attached to the reply was left behind by the pin; now the whole turn leaves.
    #[test]
    fn pulling_out_a_turn_keeps_its_table_and_gives_pictures_their_own_islands() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("FloatingIslands"));
        let table = Detail::Table { headers: vec!["App".into(), "RAM".into()], rows: vec![vec!["Heimdall".into(), "41 GB".into()], vec!["a|b".into()]] };
        let png = [0x89u8, b'P', b'N', b'G', 1, 2, 3];
        let notes = vec![("Memory".to_string(), table.clone()), ("Trend".to_string(), Detail::Svg("<svg/>".into())), ("Shot".to_string(), Detail::Image { archive_id: "f1".into() })];
        let image = |id: &str| (id == "f1").then(|| png.to_vec());
        let out = pull_out_turn(&root, "About 60 GB free.", &notes, 1_792_596_000_123, &image).unwrap();
        assert_eq!(out.len(), 3, "the document, the SVG, the image");
        let doc = std::fs::read_to_string(&out[0].1).unwrap();
        assert!(doc.starts_with("About 60 GB free.\n\n**Memory**\n\n| App | RAM |"), "{doc}");
        assert!(doc.contains("| Heimdall | 41 GB |") && doc.contains("| a\\|b |  |"), "{doc}");
        assert_eq!(root.read(&out[0].0).unwrap()["title"], json!(reply_title(1_792_596_000_123)));
        assert_eq!(out[1].1.extension().unwrap(), "svg");
        assert_eq!(root.read(&out[1].0).unwrap()["title"], json!("Trend"));
        assert_eq!(std::fs::read(&out[2].1).unwrap(), png);
        assert_eq!(out[2].1.extension().unwrap(), "png");
        // One note alone (its pin, or its row in the conversation): titled by itself, no heading inside.
        let one = pull_out_turn(&root, "", &[("Memory".into(), table)], 1_792_596_000_200, &image).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(root.read(&one[0].0).unwrap()["title"], json!("Memory"));
        assert!(std::fs::read_to_string(&one[0].1).unwrap().starts_with("| App | RAM |"));
        // Nothing to keep (an image that cannot be read) is refused.
        assert!(pull_out_turn(&root, " ", &[("Gone".into(), Detail::Image { archive_id: "nope".into() })], 1_792_596_000_300, &image).is_err());
    }

    #[test]
    fn the_listener_takes_the_first_snapshot_as_its_baseline() {
        // `listen` registers on the process-wide hub; this test only publishes events no other test's listener reads
        // as nudges (other listeners ignore attention).
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        listen(move |q| s.lock().unwrap().push(q));
        let e = |v: Value| IslandsEvent::Changed(vec![island("nudge-test-job", v)]);
        super::super::publish(&e(json!({"attention": "waiting"})));
        assert!(seen.lock().unwrap().is_empty(), "already needing User at launch: no nudge");
        super::super::publish(&e(json!({})));
        super::super::publish(&e(json!({"attention": "question"})));
        super::super::publish(&e(json!({"attention": "question", "stage": "Asking"})));
        super::super::publish(&IslandsEvent::Response(Map::new()));
        assert_eq!(*seen.lock().unwrap(), vec![true]);
    }
}
