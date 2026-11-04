//! Proof for Settled ("the best institution is the one we don't have to trust"; `docs/one-system.md`).
//! A finished piece settles with its proof, read by code from the line it came from, never from a model's word: a
//! commit, a test run, an install, an output file that exists, or, for a decision, the user's own quote. A piece with none
//! of these is claimed: drawn dashed and never counted.

use serde_json::{json, Value};

/// The proof in `text`, strongest first: commit, test, install, file (no existence or freshness checks: `near`
/// checks them for the Goals job, `plan_edit` checks what it accepts).
pub fn find(text: &str) -> Option<Value> {
    commit(text).map(|c| json!({"kind": "commit", "value": c, "label": format!("commit {}", &c[..c.len().min(8)])}))
        .or_else(|| test(text).map(|t| json!({"kind": "test", "value": t, "label": t})))
        .or_else(|| install(text).map(|t| json!({"kind": "install", "value": t, "label": t})))
        .or_else(|| file(text, None).map(|f| json!({"kind": "file", "value": f, "label": f.rsplit('/').next().unwrap_or(&f).to_owned()})))
}

/// The proof for one finished piece: only in the sentence its quote came from and the one after it (review 2 Nov: one
/// notice finishing A with a commit must not prove B, written in the next clause), a commit that exists in one of the
/// Vault's repositories, a file written within two days before `at`.
pub fn near(entry: &str, quote: &str, at: i64) -> Option<Value> {
    let w = window(entry, quote);
    commit(&w).filter(|c| commit_exists(c)).map(|c| json!({"kind": "commit", "value": c, "label": format!("commit {}", &c[..c.len().min(8)])}))
        .or_else(|| test(&w).map(|t| json!({"kind": "test", "value": t, "label": t})))
        .or_else(|| install(&w).map(|t| json!({"kind": "install", "value": t, "label": t})))
        .or_else(|| file(&w, Some(at)).map(|f| json!({"kind": "file", "value": f, "label": f.rsplit('/').next().unwrap_or(&f).to_owned()})))
}

/// Sentences: split at ". ", "; ", "! ", "? " and line breaks (never at a dot inside a path or a version).
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let next_space = chars.get(i + 1).is_none_or(|n| n.is_whitespace());
        if c == '\n' || (matches!(c, '.' | ';' | '!' | '?') && next_space) {
            if !cur.trim().is_empty() { out.push(cur.trim().to_owned()) }
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_owned()) }
    out
}

/// The sentence holding the quote's first words, and the next one; the whole entry when the quote is not found.
fn window(entry: &str, quote: &str) -> String {
    let n = crate::goals::ops::normalise;
    let head: String = n(quote).split(' ').take(4).collect::<Vec<_>>().join(" ");
    let s = sentences(entry);
    match s.iter().position(|x| !head.is_empty() && n(x).contains(&head)) {
        Some(i) => s[i..(i + 2).min(s.len())].join(". "),
        None => entry.to_owned(),
    }
}

/// The commit is in one of the project repositories under ~/Projects (read-only `git cat-file`).
pub fn commit_exists(sha: &str) -> bool {
    if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) { return false }
    let Some(home) = std::env::var_os("HOME") else { return false };
    let projects = std::path::Path::new(&home).join("Projects");
    let Ok(dirs) = std::fs::read_dir(&projects) else { return false };
    dirs.flatten().map(|e| e.path()).filter(|p| p.join(".git").is_dir()).take(80).any(|repo| {
        std::process::Command::new("git").arg("-C").arg(&repo).args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    })
}

/// The user's own words as the proof of a decision, with the time he said them.
pub fn quote(q: &str, at: i64) -> Value {
    let when = chrono::DateTime::from_timestamp(at, 0).map(|d| d.with_timezone(&chrono::Local).format("%a %H:%M").to_string()).unwrap_or_default();
    json!({"kind": "quote", "value": crate::goals::ops::cap(q, 200), "label": format!("User, {when}"), "at": at})
}

fn tokens(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '[' | ']' | '`' | '"' | '\'' | '<' | '>')).filter(|t| !t.is_empty()).collect()
}

/// A git hash (7-40 hex, a digit and a letter in it) named by a commit word nearby ("commit 3e4c208c", "merged as
/// 3e4c208c", "landed 3e4c208c", "HEAD 3e4c208c").
fn commit(text: &str) -> Option<String> {
    let t = tokens(text);
    let hex = |w: &str| {
        let w = w.trim_end_matches(['.', ':']);
        (7..=40).contains(&w.len()) && w.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            && w.chars().any(|c| c.is_ascii_digit()) && w.chars().any(|c| c.is_ascii_alphabetic())
    };
    let word = |w: &str| { let w = w.to_lowercase(); ["commit", "commits", "committed", "merged", "merge", "landed", "head", "sha", "rebased", "cherry-picked"].iter().any(|k| w.trim_end_matches([':', '.']) == *k) };
    for (i, w) in t.iter().enumerate() {
        if !hex(w) { continue }
        let before = &t[i.saturating_sub(4)..i];
        let negated = before.iter().any(|x| matches!(x.to_lowercase().as_str(), "not" | "never" | "uncommitted" | "without" | "before"));
        if before.iter().any(|x| word(x)) && !negated { return Some(w.trim_end_matches(['.', ':']).to_owned()) }
    }
    None
}

/// A passing test run: "N passed" (and no failures named), "tests pass", "test result: ok".
fn test(text: &str) -> Option<String> {
    let low = text.to_lowercase();
    let t = tokens(&low);
    // Any failure named voids it ("12 passed before it crashed", "passed locally but CI fails"), except "0 failed".
    let failed = t.iter().enumerate().any(|(i, w)| (w.starts_with("fail") || w.starts_with("crash") || w.starts_with("broke"))
        && !(i > 0 && t[i - 1] == "0"));
    if failed { return None }
    for w in t.windows(3) {
        if let Ok(n) = w[0].parse::<u64>() {
            if n > 0 && (w[1].starts_with("passed") || (w[1].starts_with("test") && w[2].starts_with("pass"))) {
                return Some(format!("{n} tests passed"));
            }
        }
    }
    if low.contains("test result: ok") { return Some("test result: ok".into()) }
    if low.contains("tests pass") || low.contains("all tests green") || low.contains("tests are green") { return Some("tests pass".into()) }
    None
}

/// An install: "installed" said of a build or an app, not "not installed" or "install it".
fn install(text: &str) -> Option<String> {
    let low = text.to_lowercase();
    let t = tokens(&low);
    for (i, w) in t.iter().enumerate() {
        if w.trim_end_matches(['.', ',', ':']) != "installed" { continue }
        let before = &t[i.saturating_sub(3)..i];
        if before.iter().any(|b| matches!(b.trim_matches(|c: char| !c.is_alphanumeric() && c != '\''), "not" | "never" | "isn't" | "wasn't" | "hasn't" | "haven't" | "didn't"
            | "can't" | "cannot" | "won't" | "un" | "no" | "be" | "been" | "to" | "will" | "should" | "must" | "needs" | "need" | "if" | "once" | "until")) { continue }
        return Some("installed".into());
    }
    None
}

/// An output file the line names that exists on this Mac (an absolute or ~ path to a file with an extension), written
/// within two days before `at` when given (an old file merely mentioned is no output).
fn file(text: &str, at: Option<i64>) -> Option<String> {
    for w in tokens(text) {
        let w = w.trim_end_matches(['.', ',', ':', '!', '?']);
        let p = match w.strip_prefix("~/") {
            Some(rest) => match std::env::var_os("HOME") { Some(h) => std::path::PathBuf::from(h).join(rest), None => continue },
            None if w.starts_with('/') => std::path::PathBuf::from(w),
            None => continue,
        };
        if p.extension().is_none() || !p.is_file() { continue }
        if let Some(at) = at {
            let m = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
            if m.is_none_or(|m| m < at - 2 * 86_400) { continue }
        }
        return Some(p.to_string_lossy().into_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_needs_its_word_and_a_hash_that_looks_like_one() {
        assert_eq!(find("Merged as 3e4c208c into main.").unwrap()["kind"], "commit");
        assert_eq!(find("commit: e009b1e1a2").unwrap()["value"], "e009b1e1a2");
        assert!(find("the colour deadbeef1 is nice").is_none(), "no commit word");
        assert!(find("commit 12345678").is_none(), "digits only is a number");
    }

    #[test]
    fn a_test_run_passes_only_without_failures() {
        assert_eq!(find("cargo test: 42 passed, 0 failed").unwrap()["label"], "42 tests passed");
        assert!(find("40 passed, 2 failed").is_none());
        assert_eq!(find("test result: ok. 12 passed").unwrap()["kind"], "test");
    }

    #[test]
    fn an_install_and_an_existing_file_count_a_bare_claim_does_not() {
        assert_eq!(find("Heimdall build installed; rollback kept").unwrap()["kind"], "install");
        assert!(find("it is not installed yet").is_none());
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("report.html");
        std::fs::write(&f, "x").unwrap();
        let v = find(&format!("The page is at {}.", f.display())).unwrap();
        assert_eq!((v["kind"].as_str(), v["label"].as_str()), (Some("file"), Some("report.html")));
        assert!(find("The page is at /nowhere/report.html").is_none());
        assert!(find("Task complete. Everything is done.").is_none(), "a claim");
    }

    #[test]
    fn only_the_quoted_sentence_proves_and_negations_and_failures_void() {
        let entry = "Arsenal tap landed in commit 3e4c208c. The Vella build is written but not built.";
        assert!(near(entry, "The Vella build is written", 0).is_none(), "the commit is the other sentence's");
        assert!(find("the build failed; 12 passed before it crashed").is_none());
        assert!(find("tests passed locally but CI fails").is_none());
        assert!(find("it hasn't been installed").is_none() && find("make sure it will be installed").is_none());
        assert!(find("not committed; HEAD 8af38bbc").is_none());
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("out.png");
        std::fs::write(&f, "x").unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!(near(&format!("Saved {}.", f.display()), "Saved", now).is_some());
        assert!(near(&format!("Saved {}.", f.display()), "Saved", now + 10 * 86_400).is_none(), "an old file mentioned is no output");
        assert!(!commit_exists("5eadbeef1"));
    }
}

