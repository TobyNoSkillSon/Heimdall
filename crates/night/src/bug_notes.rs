//! The bug-notes inbox: Heimdall files notes with its `bug_note` tool (crates/app), the user sees the count in the
//! menu, and the night fixing round resolves each note it handled into `bugs/resolved`, so the inbox ends empty.
//!
//! A note is `<id>.md` (what a person reads) plus `<id>.json` (the record), private (0600, folder 0700). The `.md`
//! appears first and the `.json` last, so a `.json` always means a complete note. `<id>` is the local time it was
//! filed (`2026-10-19T12-32-53`, then `-2`, `-3` within the same second).
//!
//! A note also carries a `cause`: its class, a one-to-three-sentence reasoning and the evidence for it.
//! The bug scout (`bug_scout`) writes it when the note is filed; the night pass routes the notes by class.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Kind of every bug-note record, old spoken ones included.
pub const KIND: &str = "bug_note";
/// What a note is about (`docs/bugs.md`, "Cause classes"). Each class has one route in `docs/nightly-build.md`.
pub const CAUSE_CLASSES: [&str; 6] = ["code", "decider", "agent-prompt", "model-quality", "environment", "unknown"];
/// The decider's answers for a wake case: the dispatch question's labels (`send`, `wait`, `not_for_heimdall`).
pub const DECIDER_LABELS: [&str; 3] = ["send", "wait", "not_for_heimdall"];
/// Who wrote a cause: the bug scout, the triage pass, or the coordinator by hand.
const WHO: [&str; 3] = ["scout", "triage", "coordinator"];

/// The configured inbox, in a code checkout where the night round works: `HEIMDALL_BUG_INBOX`, else the `bug_inbox`
/// setting in Heimdall's settings.json, else Heimdall's own checkout in the home folder
/// (`<home>/Projects/Heimdall/bugs/inbox`, when that project's `BRIEF.md` exists). None otherwise; notes then go to
/// `data_inbox`.
pub fn default_inbox() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("HEIMDALL_BUG_INBOX") {
        return Some(PathBuf::from(dir));
    }
    setting_inbox(&heimdall_home::data_dir()).or_else(|| checkout_inbox(&heimdall_home::home_dir()))
}

/// `<home>/Projects/Heimdall/bugs/inbox` when the checkout is there (its `BRIEF.md` marks it).
pub fn checkout_inbox(home: &Path) -> Option<PathBuf> {
    let project = home.join("Projects").join("Heimdall");
    project.join("BRIEF.md").is_file().then(|| project.join("bugs").join("inbox"))
}

/// The `bug_inbox` setting in `<data>/settings.json`, when set to a full path.
pub fn setting_inbox(data: &Path) -> Option<PathBuf> {
    let settings: Value = serde_json::from_slice(&std::fs::read(data.join("settings.json")).ok()?).ok()?;
    settings["bug_inbox"].as_str().map(PathBuf::from).filter(|p| p.is_absolute())
}

/// The inbox when none is configured: `bugs/inbox` in Heimdall's data folder.
pub fn data_inbox(data: &Path) -> PathBuf {
    data.join("bugs/inbox")
}

/// Where handled notes go: `bugs/resolved` beside the inbox.
pub fn resolved_dir(inbox: &Path) -> Result<PathBuf> {
    Ok(inbox.parent().context("the bug inbox has no parent folder")?.join("resolved"))
}

/// File one note. `text` is Heimdall's plain-English account; `evidence` is anything it already had (an error
/// line, an excerpt of a bad tool result). Safe to call from several threads or processes at once.
pub fn write(inbox: &Path, text: &str, evidence: Option<&str>, now_ms: u64) -> Result<Value> {
    let text = text.trim();
    ensure!(!text.is_empty(), "a bug note needs text");
    let evidence = evidence.map(str::trim).filter(|e| !e.is_empty());
    heimdall_archive::private_dir(inbox)?;
    let base = local_stamp(now_ms / 1000, false);
    let created_local = local_stamp(now_ms / 1000, true);
    for n in 1..10_000u32 {
        let id = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        if inbox.join(format!("{id}.json")).exists() {
            continue;
        }
        let mut record = json!({"schema":3,"kind":KIND,"id":id,"created_unix_ms":now_ms,"created_local":created_local,
            "filed_by":"heimdall","note":{"text":text}});
        if let Some(e) = evidence {
            record["evidence"] = json!(e);
        }
        // The summary claims the ID: a hard link fails if the name exists, so two writers never share one.
        if !claim(&inbox.join(format!("{id}.md")), summary(&record).as_bytes())? {
            continue;
        }
        heimdall_archive::atomic_write(&inbox.join(format!("{id}.json")), &serde_json::to_vec_pretty(&record)?)?;
        return Ok(record);
    }
    bail!("no free bug-note name for {base}")
}

/// Create `path` with `bytes` only if it does not exist yet; whole or not at all.
fn claim(path: &Path, bytes: &[u8]) -> Result<bool> {
    use std::io::Write;
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().context("note folder")?;
    let tmp = dir.join(format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<bool> {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        heimdall_archive::full_sync(&file)?;
        match std::fs::hard_link(&tmp, path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// The Markdown a person (or the night round) reads.
pub fn summary(r: &Value) -> String {
    let mut s = format!("# Bug note {}\n\n", r["created_local"].as_str().unwrap_or_default());
    let text = r["note"]["text"].as_str().unwrap_or_default().trim();
    s += if text.is_empty() { "(no text)" } else { text };
    s += "\n";
    if r["cause"].is_object() {
        s += &cause_section(&r["cause"]);
    }
    if let Some(e) = r["evidence"].as_str() {
        s += &format!("\n## Evidence\n\n```\n{}\n```\n", e.replace("```", "'''"));
    }
    s
}

/// Every complete note in the inbox, newest first. Unreadable files are skipped, not fatal: the menu still counts
/// the rest.
pub fn list(inbox: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = std::fs::read_dir(inbox)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice::<Value>(&std::fs::read(&p).ok()?).ok())
        .filter(|v| v["kind"] == KIND)
        .collect();
    out.sort_by(|a, b| {
        (b["created_unix_ms"].as_u64(), b["id"].as_str()).cmp(&(a["created_unix_ms"].as_u64(), a["id"].as_str()))
    });
    out
}

/// Open notes that `text` (a commit message, a log) names by ID. A fix that names its note but leaves the note in the
/// inbox is the stale-note case `bugs check` stops.
pub fn cited(inbox: &Path, text: &str) -> Vec<String> {
    list(inbox)
        .iter()
        .filter_map(|n| n["id"].as_str())
        .filter(|id| mentions(text, id))
        .map(str::to_owned)
        .collect()
}

/// `id` appears in `text` as a whole word: `2026-10-22T23-26-13` is not part of `2026-10-22T23-26-13-2`.
fn mentions(text: &str, id: &str) -> bool {
    text.match_indices(id).any(|(at, _)| {
        !text[at + id.len()..].chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Resolve every open note a commit already on `main` names: the fix landed, so the note closes with that commit.
/// `commits` is `(short sha, subject, message)`, newest first, as `git log` gives them; the newest naming commit is
/// cited, with any older ones. Returns `(id, resolution)` for each note it resolved.
pub fn reconcile(inbox: &Path, commits: &[(String, String, String)], now_ms: u64) -> Result<Vec<(String, String)>> {
    let mut done = Vec::new();
    for id in list(inbox).iter().filter_map(|n| n["id"].as_str().map(str::to_owned)) {
        let naming: Vec<&(String, String, String)> = commits.iter().filter(|(_, _, msg)| mentions(msg, &id)).collect();
        let Some((sha, subject, _)) = naming.first() else { continue };
        let mut text = format!("Fixed on main by {sha}: {subject}");
        if naming.len() > 1 {
            let more: Vec<&str> = naming[1..].iter().map(|(s, _, _)| s.as_str()).collect();
            text += &format!(" (also named by {})", more.join(", "));
        }
        text += ". Resolved by `bugs reconcile`.";
        resolve(inbox, &id, &text, now_ms)?;
        done.push((id, text));
    }
    Ok(done)
}

/// The night round handled note `id`: move it to `bugs/resolved` with what was done, leaving nothing in the inbox.
/// Returns the resolved `.md`.
pub fn resolve(inbox: &Path, id: &str, resolution: &str, now_ms: u64) -> Result<PathBuf> {
    let resolution = resolution.trim();
    ensure!(!resolution.is_empty(), "say what was done for {id}");
    ensure_id(id)?;
    let (json_path, md_path) = (inbox.join(format!("{id}.json")), inbox.join(format!("{id}.md")));
    let record: Option<Value> = match std::fs::read(&json_path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).with_context(|| format!("{id}.json"))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let md = match std::fs::read_to_string(&md_path) {
        Ok(md) => Some(md),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if record.is_none() && md.is_none() {
        bail!("no bug note {id} in {}", inbox.display());
    }
    let resolved = resolved_dir(inbox)?;
    heimdall_archive::private_dir(&resolved)?;
    let mut target = id.to_string();
    let mut n = 2;
    while resolved.join(format!("{target}.json")).exists() || resolved.join(format!("{target}.md")).exists() {
        target = format!("{id}-r{n}");
        n += 1;
    }
    let when = local_stamp(now_ms / 1000, true);
    let mut md = md.unwrap_or_else(|| record.as_ref().map(summary).unwrap_or_default());
    if !md.ends_with('\n') {
        md.push('\n');
    }
    md += &format!("\n## Resolved {when}\n\n{resolution}\n");
    let out = resolved.join(format!("{target}.md"));
    heimdall_archive::atomic_write(&out, md.as_bytes())?;
    if let Some(mut record) = record {
        record["resolution"] = json!({"text":resolution,"resolved_unix_ms":now_ms,"resolved_local":when});
        heimdall_archive::atomic_write(&resolved.join(format!("{target}.json")), &serde_json::to_vec_pretty(&record)?)?;
    }
    // The record goes first: the note leaves the menu count before its summary leaves the folder.
    for path in [&json_path, &md_path] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// Resolve every handled note; notes not in `handled` stay in the inbox. Returns how many were resolved.
pub fn resolve_all(inbox: &Path, handled: &[(&str, &str)], now_ms: u64) -> Result<usize> {
    for (id, resolution) in handled {
        resolve(inbox, id, resolution, now_ms)?;
    }
    Ok(handled.len())
}

fn ensure_id(id: &str) -> Result<()> {
    ensure!(!id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.'), "not a bug-note id: {id}");
    Ok(())
}

/// The record of open note `id` (the inbox only: a resolved note has its class already).
pub fn get(inbox: &Path, id: &str) -> Result<Value> {
    ensure_id(id)?;
    let bytes = std::fs::read(inbox.join(format!("{id}.json"))).with_context(|| format!("no bug note {id} with a record in {}", inbox.display()))?;
    let record: Value = serde_json::from_slice(&bytes).with_context(|| format!("{id}.json"))?;
    ensure!(record["kind"] == KIND, "{id} is not a bug note");
    Ok(record)
}

/// Checks a cause and keeps only its known fields. Every class needs a reasoning of one to three sentences, a
/// confidence between 0 and 1 and a writer; a `decider` case needs a label the decider answers with.
pub fn check_cause(cause: &Value) -> Result<Value> {
    let class = cause["class"].as_str().context("a cause needs a class")?;
    ensure!(CAUSE_CLASSES.contains(&class), "cause class {class:?} is not one of {CAUSE_CLASSES:?}");
    let reasoning = cause["reasoning"].as_str().unwrap_or_default().trim();
    ensure!(!reasoning.is_empty(), "a cause needs its reasoning");
    ensure!(reasoning.len() <= 800, "cause reasoning is longer than 800 bytes");
    ensure!(sentences(reasoning) <= 3, "cause reasoning is more than three sentences");
    let confidence = cause["confidence"].as_f64().context("a cause needs a confidence between 0 and 1")?;
    ensure!((0.0..=1.0).contains(&confidence), "cause confidence {confidence} is not between 0 and 1");
    let by = cause["by"].as_str().unwrap_or("coordinator");
    ensure!(WHO.contains(&by), "cause by {by:?} is not one of {WHO:?}");
    let mut out = json!({"class": class, "reasoning": reasoning, "confidence": confidence, "by": by});
    for key in ["evidence", "suspected_files", "suspected_functions"] {
        let items: Vec<String> = cause[key].as_array().into_iter().flatten().filter_map(Value::as_str)
            .map(str::trim).filter(|s| !s.is_empty()).take(8).map(str::to_owned).collect();
        out[key] = json!(items);
    }
    out["repro_hint"] = json!(cause["repro_hint"].as_str().unwrap_or_default().trim());
    out["suspected_sites"] = json!(cause["suspected_sites"].as_array().into_iter().flatten().filter(|s| s.is_object()).take(8).cloned().collect::<Vec<_>>());
    for key in ["model", "at_unix_ms", "cost_usd", "scout_version"] {
        if !cause[key].is_null() {
            out[key] = cause[key].clone();
        }
    }
    if class == "decider" && !cause["decider_case"].is_null() {
        let label = cause["decider_case"]["label"].as_str().unwrap_or_default();
        ensure!(DECIDER_LABELS.contains(&label), "decider label {label:?} is not one of {DECIDER_LABELS:?}");
        out["decider_case"] = cause["decider_case"].clone();
    }
    Ok(out)
}

/// The first `n` sentences of `text`, punctuation kept (all of it when it has fewer).
pub fn first_sentences(text: &str, n: usize) -> String {
    let chars: Vec<char> = text.trim().chars().collect();
    let mut seen = 0;
    for (i, c) in chars.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace()) {
            seen += 1;
            if seen == n {
                return chars[..=i].iter().collect();
            }
        }
    }
    chars.iter().collect()
}

/// About the number of sentences: full stops, question or exclamation marks followed by a space or the end. A dot
/// inside a path or a number (`bugs.rs`, `22.5`) does not end one.
fn sentences(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).filter(|&i| matches!(chars[i], '.' | '!' | '?') && chars.get(i + 1).is_none_or(|c| c.is_whitespace())).count()
}

/// Writes `cause` into open note `id` (checked by `check_cause`) and rewrites its `.md` to show it. A note keeps its
/// last cause: the night pass or the coordinator may replace a scout's.
pub fn set_cause(inbox: &Path, id: &str, cause: &Value) -> Result<Value> {
    let mut record = get(inbox, id)?;
    record["cause"] = check_cause(cause).with_context(|| format!("cause for {id}"))?;
    heimdall_archive::atomic_write(&inbox.join(format!("{id}.json")), &serde_json::to_vec_pretty(&record)?)?;
    heimdall_archive::atomic_write(&inbox.join(format!("{id}.md")), summary(&record).as_bytes())?;
    Ok(record)
}

fn cause_section(c: &Value) -> String {
    let names = |key: &str| c[key].as_array().into_iter().flatten().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
    let mut s = format!(
        "\n## Cause: {} (confidence {:.2}, by {})\n\n{}\n",
        c["class"].as_str().unwrap_or("unknown"),
        c["confidence"].as_f64().unwrap_or(0.0),
        c["by"].as_str().unwrap_or("?"),
        c["reasoning"].as_str().unwrap_or_default()
    );
    for site in c["suspected_sites"].as_array().into_iter().flatten() {
        s += &format!(
            "\nSite: {} {} lines {}\n",
            site["file"].as_str().unwrap_or_default(),
            site["function"].as_str().unwrap_or_default(),
            site["lines"].as_str().unwrap_or_default()
        );
    }
    for (label, key) in [("Files", "suspected_files"), ("Functions", "suspected_functions")] {
        if !names(key).is_empty() {
            s += &format!("\n{label}: {}\n", names(key));
        }
    }
    if let Some(hint) = c["repro_hint"].as_str().filter(|h| !h.is_empty()) {
        s += &format!("\nRepro: {hint}\n");
    }
    if let Some(label) = c["decider_case"]["label"].as_str() {
        s += &format!("\nDecider label: {label}\n");
    }
    s
}

/// Decider cases: every note, open or resolved, whose cause is the decider's with a labelled case. Newest first.
pub fn decider_cases(inbox: &Path) -> Vec<Value> {
    let mut all = list(inbox);
    if let Ok(resolved) = resolved_dir(inbox) {
        all.extend(list(&resolved));
    }
    all.into_iter()
        .filter(|n| n["cause"]["class"] == "decider" && n["cause"]["decider_case"]["label"].is_string())
        .collect()
}

/// One decider case as a row of the decider dataset (`lab/decider-v4/DATASET.md`). The window is the transcript the
/// archive held around the note; `state` is the note and that window, not the decider's own prompt bytes, so
/// `state_exact` is false: the next decider build refits the state from the journal before training on the row.
pub fn decider_row(note: &Value) -> Value {
    let (id, case, cause) = (note["id"].as_str().unwrap_or_default(), &note["cause"]["decider_case"], &note["cause"]);
    let window = case["window"].clone();
    json!({
        "id": format!("bug:{id}"),
        "session": format!("bug-note:{id}"),
        "point": id,
        "task": "dispatch",
        "family": format!("bug:{id}"),
        "style": "bug-note",
        "origin": "bug-note",
        "source": "bug-note",
        "split": "bug_cases",
        "variant": case["variant"].as_str().unwrap_or("unknown"),
        "label": case["label"],
        "label_source": cause["by"],
        "state_exact": false,
        "state": json!({"note": note["note"]["text"], "window": window}).to_string(),
        "reasoning": cause["reasoning"],
        "confidence": cause["confidence"],
        "eligible": true,
    })
}

fn local(secs: u64) -> libc::tm {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// `2026-10-12T14-05-11` (file-name safe) or `2026-10-12 14:05:11` (display), local time.
pub fn local_stamp(secs: u64, display: bool) -> String {
    let t = local(secs);
    let (sep, c) = if display { (' ', ':') } else { ('T', '-') };
    format!("{}-{:02}-{:02}{sep}{:02}{c}{:02}{c}{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: u64 = 1_792_406_000_000;

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map(|d| d.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn the_inbox_is_in_the_data_folder_unless_a_checkout_is_configured() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(data_inbox(t.path()), t.path().join("bugs/inbox"));
        assert_eq!(setting_inbox(t.path()), None);
        std::fs::write(t.path().join("settings.json"), r#"{"bug_inbox":"relative/inbox"}"#).unwrap();
        assert_eq!(setting_inbox(t.path()), None, "only a full path");
        std::fs::write(t.path().join("settings.json"), r#"{"dry_run":true,"bug_inbox":"/code/bugs/inbox"}"#).unwrap();
        assert_eq!(setting_inbox(t.path()), Some(PathBuf::from("/code/bugs/inbox")));
    }

    #[test]
    fn a_note_is_plain_text_with_its_evidence_private_and_complete() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let text = "Zażółć gęślą jaźń — the t3_workers result was a wall of JSON 🧱 at 12:32; expected one line per worker.";
        let r = write(&inbox, &format!("  {text}\n"), Some("{\"workers\":[…]}"), NOW).unwrap();
        let id = r["id"].as_str().unwrap();
        assert_eq!(files(&inbox), [format!("{id}.json"), format!("{id}.md")]);
        let saved: Value = serde_json::from_slice(&std::fs::read(inbox.join(format!("{id}.json"))).unwrap()).unwrap();
        assert_eq!(saved, r);
        assert_eq!(saved["note"]["text"], text);
        assert_eq!(saved["kind"], KIND);
        let md = std::fs::read_to_string(inbox.join(format!("{id}.md"))).unwrap();
        assert!(md.contains(text) && md.contains("## Evidence") && md.contains("{\"workers\":[…]}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&inbox).unwrap().permissions().mode() & 0o777, 0o700);
            for f in files(&inbox) {
                assert_eq!(std::fs::metadata(inbox.join(f)).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        assert!(write(&inbox, "   ", None, NOW).is_err());
        // Blank evidence is no evidence.
        let r = write(&inbox, "x", Some("  "), NOW).unwrap();
        assert!(r.get("evidence").is_none());
    }

    #[test]
    fn a_commit_naming_an_open_note_is_reported_until_the_note_is_resolved() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let first = write(&inbox, "first", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        // Same second, so the second note's ID is `<first>-2`.
        let second = write(&inbox, "second", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        assert_eq!(second, format!("{first}-2"));
        assert_eq!(cited(&inbox, &format!("Fixes bug {first}.")), [first.clone()]);
        assert_eq!(cited(&inbox, &format!("Fixes bug {second}.")), [second.clone()]);
        assert!(cited(&inbox, "no note named here").is_empty());
        resolve(&inbox, &first, "fixed in abc1234", NOW).unwrap();
        assert!(cited(&inbox, &format!("Fixes bug {first}.")).is_empty());
    }

    #[test]
    fn a_note_whose_fix_is_on_main_is_resolved_with_that_commit_and_the_rest_stay() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let fixed = write(&inbox, "fixed", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        let sibling = write(&inbox, "same second", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        let open = write(&inbox, "open", None, NOW + 5_000).unwrap()["id"].as_str().unwrap().to_owned();
        let c = |sha: &str, msg: String| (sha.to_owned(), msg.lines().next().unwrap().to_owned(), msg);
        let commits = [
            c("bbb2222", format!("Test: pin it\n\nFixes bug note {fixed}.")),
            c("ccc3333", "Unrelated change".to_owned()),
            c("aaa1111", format!("Fix the thing\n\nFixes bug note {fixed}.")),
        ];
        let done = reconcile(&inbox, &commits, NOW).unwrap();
        // `<fixed>-2` is a different note: naming `<fixed>` doesn't close it.
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, fixed);
        assert!(done[0].1.starts_with("Fixed on main by bbb2222: Test: pin it") && done[0].1.contains("also named by aaa1111"));
        let open_ids: Vec<String> = list(&inbox).iter().map(|n| n["id"].as_str().unwrap().to_owned()).collect();
        assert!(open_ids.contains(&sibling) && open_ids.contains(&open) && !open_ids.contains(&fixed));
        let md = std::fs::read_to_string(resolved_dir(&inbox).unwrap().join(format!("{fixed}.md"))).unwrap();
        assert!(md.contains("Fixed on main by bbb2222"));
        assert!(reconcile(&inbox, &commits, NOW).unwrap().is_empty(), "a second run resolves nothing more");
    }

    #[test]
    fn long_text_is_kept_whole() {
        let t = tempfile::tempdir().unwrap();
        let text = "Ślad błędu. ".repeat(3000);
        let evidence = "line ```fenced``` ".repeat(5000);
        let r = write(t.path(), &text, Some(&evidence), NOW).unwrap();
        let back = &list(t.path())[0];
        assert_eq!(back["note"]["text"], text.trim());
        assert_eq!(back["evidence"], evidence.trim());
        let md = std::fs::read_to_string(t.path().join(format!("{}.md", r["id"].as_str().unwrap()))).unwrap();
        // The evidence cannot close its own code block.
        assert_eq!(md.matches("```").count(), 2);
    }

    #[test]
    fn concurrent_notes_in_one_second_get_distinct_complete_entries() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().to_path_buf();
        let ids: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..24)
                .map(|i| {
                    let inbox = &inbox;
                    s.spawn(move || write(inbox, &format!("note {i}"), None, NOW).unwrap()["id"].as_str().unwrap().to_owned())
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 24);
        let listed = list(&inbox);
        assert_eq!(listed.len(), 24);
        let mut texts: Vec<_> = listed.iter().map(|r| r["note"]["text"].as_str().unwrap().to_owned()).collect();
        texts.sort();
        let mut expected: Vec<_> = (0..24).map(|i| format!("note {i}")).collect();
        expected.sort();
        assert_eq!(texts, expected);
        // No temporary files left behind.
        assert!(files(&inbox).iter().all(|f| f.ends_with(".json") || f.ends_with(".md")));
        assert_eq!(files(&inbox).len(), 48);
    }

    #[test]
    fn list_is_newest_first_counts_only_bug_notes_and_skips_damage() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path();
        write(inbox, "older", None, NOW).unwrap();
        write(inbox, "newer", None, NOW + 60_000).unwrap();
        std::fs::write(inbox.join("broken.json"), b"{not json").unwrap();
        std::fs::write(inbox.join("review.json"), br#"{"kind":"command_decision_auto","id":"review"}"#).unwrap();
        std::fs::write(inbox.join("loose.md"), b"# a note someone typed").unwrap();
        // A spoken note from before the tool (schema 2) still counts.
        std::fs::write(inbox.join("2026-10-19T12-32-53.json"), br#"{"schema":2,"kind":"bug_note","id":"2026-10-19T12-32-53","created_unix_ms":1,"note":{"text":""}}"#).unwrap();
        let texts: Vec<_> = list(inbox).iter().map(|r| r["note"]["text"].as_str().unwrap().to_owned()).collect();
        assert_eq!(texts, ["newer", "older", ""]);
        assert!(list(&inbox.join("absent")).is_empty());
    }

    #[test]
    fn night_round_clears_handled_notes_and_keeps_the_rest() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let a = write(&inbox, "speak returned a wall of JSON", None, NOW).unwrap();
        let b = write(&inbox, "island stuck on Working", Some("log line"), NOW).unwrap();
        let c = write(&inbox, "not fixed tonight", None, NOW).unwrap();
        // A hand-written note without a record is resolvable too.
        std::fs::write(inbox.join("2026-10-18T16-45-47.md"), "# Bug note (from User via coordinator)\n\nJarvis voice missing").unwrap();
        let (a, b, c) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap(), c["id"].as_str().unwrap());
        let n = resolve_all(&inbox, &[(a, "Fixed in abc123: one line per result."), (b, "Fixed: the glyph ends with the turn."), ("2026-10-18T16-45-47", "Jarvis is loaded first.")], NOW + 1000).unwrap();
        assert_eq!(n, 3);
        assert_eq!(files(&inbox), [format!("{c}.json"), format!("{c}.md")]);
        assert_eq!(list(&inbox).len(), 1);
        let resolved = t.path().join("bugs/resolved");
        assert_eq!(files(&resolved).len(), 5);
        let record: Value = serde_json::from_slice(&std::fs::read(resolved.join(format!("{a}.json"))).unwrap()).unwrap();
        assert_eq!(record["resolution"]["text"], "Fixed in abc123: one line per result.");
        assert_eq!(record["note"]["text"], "speak returned a wall of JSON");
        let md = std::fs::read_to_string(resolved.join(format!("{b}.md"))).unwrap();
        assert!(md.contains("island stuck on Working") && md.contains("## Resolved") && md.ends_with("Fixed: the glyph ends with the turn.\n"));
        assert!(std::fs::read_to_string(resolved.join("2026-10-18T16-45-47.md")).unwrap().contains("Jarvis is loaded first."));
        // Resolving the last one empties the inbox completely.
        resolve(&inbox, c, "Fixed too.", NOW + 2000).unwrap();
        assert!(files(&inbox).is_empty());
        // Unknown ids, blank resolutions and paths are refused without touching anything.
        assert!(resolve(&inbox, c, "again", NOW).is_err());
        let d = write(&inbox, "d", None, NOW).unwrap();
        let d = d["id"].as_str().unwrap();
        assert!(resolve(&inbox, d, "  ", NOW).is_err());
        assert!(resolve(&inbox, "../resolved/x", "done", NOW).is_err());
        assert_eq!(list(&inbox).len(), 1);
    }

    #[test]
    fn a_resolved_name_that_exists_is_not_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("inbox");
        let first = write(&inbox, "first", None, NOW).unwrap();
        let id = first["id"].as_str().unwrap().to_owned();
        resolve(&inbox, &id, "done once", NOW).unwrap();
        // Same second again (a later note after the first moved out).
        let again = write(&inbox, "second", None, NOW).unwrap();
        assert_eq!(again["id"], id.as_str());
        let out = resolve(&inbox, &id, "done twice", NOW).unwrap();
        assert!(out.ends_with(format!("{id}-r2.md")));
        assert!(std::fs::read_to_string(t.path().join(format!("resolved/{id}.md"))).unwrap().contains("done once"));
    }

    #[test]
    fn a_cause_is_checked_kept_short_and_shown_in_the_note() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("inbox");
        let id = write(&inbox, "Plan islands glitch on refresh.", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        let cause = json!({"class": "code", "reasoning": "The refresher ticks each item with its own sound. One batch would make one.",
            "confidence": 0.8, "by": "coordinator", "evidence": ["13:51 ticked 10 items"], "suspected_files": ["crates/app/src/plan_refresher.rs"],
            "suspected_functions": ["apply_ticks"], "repro_hint": "tick ten items in one pass", "secret_extra": "dropped"});
        let saved = set_cause(&inbox, &id, &cause).unwrap();
        assert_eq!(saved["cause"]["class"], "code");
        assert!(saved["cause"].get("secret_extra").is_none());
        assert_eq!(list(&inbox)[0]["cause"]["confidence"], 0.8);
        let md = std::fs::read_to_string(inbox.join(format!("{id}.md"))).unwrap();
        assert!(md.contains("## Cause: code (confidence 0.80, by coordinator)") && md.contains("Files: crates/app/src/plan_refresher.rs") && md.contains("Repro: tick ten items"));
        // Refused: an unknown class, no reasoning, four sentences, a confidence outside 0 to 1, an unknown writer, an unknown decider label.
        for bad in [
            json!({"class": "mystery", "reasoning": "x", "confidence": 0.5}),
            json!({"class": "code", "reasoning": " ", "confidence": 0.5}),
            json!({"class": "code", "reasoning": "A. B. C. D.", "confidence": 0.5}),
            json!({"class": "code", "reasoning": "x", "confidence": 1.5}),
            json!({"class": "code", "reasoning": "x", "confidence": 0.5, "by": "a friend"}),
            json!({"class": "decider", "reasoning": "x", "confidence": 0.5, "decider_case": {"label": "maybe"}}),
        ] {
            assert!(set_cause(&inbox, &id, &bad).is_err(), "{bad}");
        }
        assert_eq!(get(&inbox, &id).unwrap()["cause"]["class"], "code", "a refused cause leaves the last one");
        assert!(set_cause(&inbox, "../x", &cause).is_err());
    }

    #[test]
    fn a_long_reasoning_keeps_its_first_three_sentences() {
        assert_eq!(first_sentences("One. Two. Three. Four.", 3), "One. Two. Three.");
        assert_eq!(first_sentences("It is 0.72 at 22:45:44. Done.", 3), "It is 0.72 at 22:45:44. Done.");
        assert_eq!(first_sentences("Open bugs.rs and stop.", 1), "Open bugs.rs and stop.");
    }

    #[test]
    fn decider_cases_export_only_labelled_decider_notes_open_or_resolved() {
        let t = tempfile::tempdir().unwrap();
        let inbox = t.path().join("bugs/inbox");
        let wake = write(&inbox, "User said shut up and Heimdall acked.", None, NOW).unwrap()["id"].as_str().unwrap().to_owned();
        let code = write(&inbox, "Island stuck.", None, NOW + 1000).unwrap()["id"].as_str().unwrap().to_owned();
        let old = write(&inbox, "Decider woke late.", None, NOW + 2000).unwrap()["id"].as_str().unwrap().to_owned();
        let case = json!({"label": "not_for_heimdall", "variant": "ack", "window": [{"time": "22:45:50", "speaker": "User", "text": "shut up"}]});
        set_cause(&inbox, &wake, &json!({"class": "decider", "reasoning": "Named only to be quiet.", "confidence": 0.9, "decider_case": case})).unwrap();
        set_cause(&inbox, &code, &json!({"class": "code", "reasoning": "A state bug.", "confidence": 0.6})).unwrap();
        set_cause(&inbox, &old, &json!({"class": "decider", "reasoning": "Late wake.", "confidence": 0.6, "decider_case": {"label": "send"}})).unwrap();
        resolve(&inbox, &old, "Exported as a decider case.", NOW + 3000).unwrap();
        let cases = decider_cases(&inbox);
        assert_eq!(cases.len(), 2, "the code note is not a decider case");
        let row = decider_row(cases.iter().find(|n| n["id"] == wake.as_str()).unwrap());
        assert_eq!(row["id"], format!("bug:{wake}"));
        assert_eq!(row["label"], "not_for_heimdall");
        assert_eq!(row["variant"], "ack");
        assert_eq!(row["split"], "bug_cases");
        assert_eq!(row["state_exact"], false);
        let state: Value = serde_json::from_str(row["state"].as_str().unwrap()).unwrap();
        assert_eq!(state["window"][0]["text"], "shut up");
    }
}

#[cfg(test)]
mod checkout_inbox_tests {
    #[test]
    fn the_checkout_in_home_is_the_inbox_only_when_its_brief_exists() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("Projects").join("Heimdall");
        std::fs::create_dir_all(&project).unwrap();
        assert_eq!(super::checkout_inbox(home.path()), None);
        std::fs::write(project.join("BRIEF.md"), "").unwrap();
        assert_eq!(super::checkout_inbox(home.path()), Some(project.join("bugs").join("inbox")));
    }
}
