//! No hard-coded owner's name in how Heimdall talks (14 Nov): the name comes from the personalization. Every prompt file, in both editions, and the runtime strings of the relay, the suggester,
//! the notices and the tools the agent reads name the user through `{user}` / `identity::user()`, which Personalise's
//! `user_name` fills. Credits (the developer's GitHub account `TobyNoSkillSon`, `tobynoskillson.com`, bundle ids) are
//! other words and pass; the few internal labels that remain are listed in `ALLOWED` with why.
use std::path::{Path, PathBuf};

/// The repository root (this crate is `crates/agent`).
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Whether `text` has the word "toby" in any case ("Toby's" counts; the developer's account and site names do not).
fn names_owner(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case("toby"))
}

/// Every prompt file under `dir`, recursively.
fn prompt_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            prompt_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

/// The skills index (`<available_skills>` … in the relay's generated prompt) lists the bundled skills' own
/// descriptions, which the skill files own; the relay replaces it at runtime with the profile's (`relay_prompt::text`).
fn without_skills_index(text: &str) -> String {
    match (text.find(agent::skill_index::START), text.rfind(agent::skill_index::END)) {
        (Some(start), Some(end)) if start < end => format!("{}{}", &text[..start], &text[end..]),
        _ => text.to_string(),
    }
}

#[test]
fn no_prompt_file_names_the_owner() {
    let mut files = Vec::new();
    for dir in ["crates/agent/prompt", "crates/app/prompt"] {
        prompt_files(&root().join(dir), &mut files);
    }
    assert!(files.len() >= 15, "found only {} prompt files", files.len());
    let mut found = Vec::new();
    for file in &files {
        let text = without_skills_index(&std::fs::read_to_string(file).unwrap());
        for (n, line) in text.lines().enumerate() {
            if names_owner(line) {
                found.push(format!("{}:{}: {line}", file.strip_prefix(root()).unwrap_or(file).display(), n + 1));
            }
        }
    }
    assert!(found.is_empty(), "write {{user}} (the configured name) instead:\n{}", found.join("\n"));
}

/// The general skills ship in every edition and the public repository (docs/skills.md, "Two layers"): none names the
/// owner either. Skill files are not filled, so they say "the user".
#[test]
fn no_general_skill_names_the_owner() {
    let mut files = Vec::new();
    for dir in ["skills/public", "skills/shared"] {
        prompt_files(&root().join(dir), &mut files);
    }
    assert!(files.len() >= 10, "found only {} skill files", files.len());
    let mut found = Vec::new();
    for file in &files {
        for (n, line) in std::fs::read_to_string(file).unwrap().lines().enumerate() {
            if names_owner(line) {
                found.push(format!("{}:{}: {line}", file.strip_prefix(root()).unwrap_or(file).display(), n + 1));
            }
        }
    }
    assert!(found.is_empty(), "a general skill says \"the user\"; one user's own rules go in their private folder:\n{}", found.join("\n"));
}

/// The runtime text of the relay (emergency/relay mode), the suggester, the agent's notices, the tool descriptions and
/// results, T3's notices and the deny list's refusals.
const RUNTIME: [&str; 31] = [
    "crates/agent/src/actions.rs",
    "crates/agent/src/emergency.rs",
    "crates/agent/src/maintenance.rs",
    "crates/agent/src/mcp_contract.rs",
    "crates/agent/src/notice.rs",
    "crates/agent/src/wakes.rs",
    "crates/app/src/bridge/questions.rs",
    "crates/app/src/bridge/speech_policy.rs",
    "crates/app/src/bridge/unread.rs",
    "crates/app/src/day_timeline.rs",
    "crates/app/src/drops.rs",
    "crates/app/src/emergency.rs",
    "crates/app/src/mcp_actions.rs",
    "crates/app/src/pipeline/listen_to.rs",
    "crates/app/src/pipeline/suggest.rs",
    "crates/app/src/resource_watch.rs",
    "crates/app/src/settings_tool.rs",
    "crates/app/src/suggester.rs",
    "crates/app/src/voice_namer.rs",
    "crates/app/src/voice_suggest.rs",
    "crates/computer/src/asks.rs",
    "crates/computer/src/lib.rs",
    "crates/emergency-cli/src/approvals.rs",
    "crates/emergency-cli/src/deny.rs",
    "crates/emergency-cli/src/grants.rs",
    "crates/islands/src/cli/usage.rs",
    "crates/t3/src/observer.rs",
    "crates/t3/src/orchestrator.rs",
    "crates/t3/src/text.rs",
    "crates/t3/src/threads.rs",
    "crates/t3/src/workers.rs",
];

/// Lines that may still say it, each with why: internal labels and compatibility reads, never words to the user.
const ALLOWED: &[(&str, &str)] = &[
    // An abort reason Pi sessions from before 14 Nov carry: still read as "not the provider failing".
    ("crates/agent/src/emergency.rs", "\"stopped by toby\""),
    // Phrases that workers (whose own prompts may name their user) write when something waits on the user: read only.
    ("crates/t3/src/text.rs", "\"toby's yes\""),
    ("crates/t3/src/text.rs", "\"waiting on toby\""),
    ("crates/t3/src/text.rs", "\"nothing for toby\""),
    ("crates/t3/src/text.rs", "\"for toby to\""),
    ("crates/t3/src/text.rs", "\"toby to decide\""),
    ("crates/t3/src/text.rs", "\"waiting for toby\""),
    ("crates/t3/src/text.rs", "\"waits on toby\""),
    ("crates/t3/src/text.rs", "\"waits for toby\""),
];

/// Whether `rest` (the text right after a `#[cfg(test)]`) is an inline test module (`mod tests {`), never a test-only
/// field, helper or `mod tests;` declaration.
fn opens_test_module(rest: &str) -> bool {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("pub(crate) ").or_else(|| rest.strip_prefix("pub ")).unwrap_or(rest);
    rest.starts_with("mod ") && rest.find('{').is_some_and(|b| rest.find(';').is_none_or(|s| b < s))
}

/// The string literals of a source file's code, each line of each with its line number. Comments, identifiers and the
/// bodies of inline test modules (`#[cfg(test)] mod … { … }`, wherever they sit in the file) are left out.
fn string_lines(text: &str) -> Vec<(usize, String)> {
    const TEST: &str = "#[cfg(test)]";
    let chars: Vec<char> = text.chars().collect();
    let (mut out, mut line, mut i) = (Vec::new(), 1, 0);
    let mut current: Option<(usize, String)> = None;
    // Brace depth, and the depth a test module's body opened at while one is being skipped.
    let (mut depth, mut skip_at): (usize, Option<usize>) = (0, None);
    let mut keep = |out: &mut Vec<(usize, String)>, skipping: bool, at: usize, s: &str| {
        if !skipping { for (k, l) in s.split('\n').enumerate() { out.push((at + k, l.to_owned())); } }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if let Some((start, s)) = current.as_mut() {
            match c {
                '\\' => { s.push(c); if let Some(n) = next { s.push(n); if n == '\n' { line += 1; } } i += 2; continue; }
                '"' => { keep(&mut out, skip_at.is_some(), *start, s); current = None; }
                '\n' => { s.push(c); line += 1; }
                _ => s.push(c),
            }
            i += 1;
            continue;
        }
        match c {
            '\n' => line += 1,
            '#' if skip_at.is_none() && chars[i..].iter().take(TEST.len()).collect::<String>() == TEST
                && opens_test_module(&chars[i + TEST.len()..].iter().take(200).collect::<String>()) => {
                skip_at = Some(depth);
                i += TEST.len();
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if skip_at == Some(depth) { skip_at = None; }
            }
            '/' if next == Some('/') => { while i < chars.len() && chars[i] != '\n' { i += 1; } continue; }
            '/' if next == Some('*') => {
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') { if chars[i] == '\n' { line += 1; } i += 1; }
                i += 2;
                continue;
            }
            // A char literal ('"', '\'', 'x'); a lifetime ('a) has no closing quote right after.
            '\'' if next == Some('\\') => { i += 3; while i < chars.len() && chars[i] != '\'' { i += 1; } }
            // A raw string (r"…", r#"…"#): its text to the closing quote and as many hashes.
            'r' if (next == Some('"') || next == Some('#')) && !i.checked_sub(1).is_some_and(|p| chars[p].is_alphanumeric() || chars[p] == '_') => {
                let hashes = chars[i + 1..].iter().take_while(|&&h| h == '#').count();
                if chars.get(i + 1 + hashes) == Some(&'"') {
                    let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
                    let body: String = chars[i + 2 + hashes..].iter().collect();
                    let len = body.find(&close).unwrap_or(body.len());
                    keep(&mut out, skip_at.is_some(), line, &body[..len]);
                    line += body[..len].matches('\n').count();
                    i += 2 + hashes + body[..len].chars().count() + close.chars().count();
                    continue;
                }
            }
            '\'' if chars.get(i + 2) == Some(&'\'') => i += 2,
            '"' => current = Some((line, String::new())),
            _ => {}
        }
        i += 1;
    }
    out
}

#[test]
fn the_relay_suggester_and_notice_strings_never_name_the_owner() {
    let mut found = Vec::new();
    for file in RUNTIME {
        let text = std::fs::read_to_string(root().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        for (n, line) in string_lines(&text) {
            if names_owner(&line) && !ALLOWED.iter().any(|(f, s)| *f == file && s.trim_matches('"') == line.trim()) {
                found.push(format!("{file}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "name the user with agent::identity::user() or {{user}}:\n{}", found.join("\n"));
}

/// The check itself: the word in any case, never the developer's account or site.
#[test]
fn the_check_finds_the_word_and_passes_credits() {
    assert!(names_owner("Tell Toby.") && names_owner("Toby's Mac") && names_owner("toby-hotkey") && names_owner("TOBY"));
    assert!(!names_owner("github.com/TobyNoSkillSon/Heimdall") && !names_owner("com.tobynoskillson.heimdall") && !names_owner("{user}"));
    // A test-only field and a `mod x;` declaration are code; an inline test module, wherever it sits, is skipped.
    let lines = string_lines("#[cfg(test)]\nf: &str = \"Toby\";\n#[cfg(test)]\nmod t { fn f() { \"in a test\"; } }\n#[cfg(test)]\nmod u;\nconst A: &str = \"after\";");
    assert_eq!(lines, [(2, "Toby".to_owned()), (7, "after".to_owned())]);
    let lines = string_lines("// Toby's note\nlet toby = \"Tell {user}.\\\"\";\nconst Q: char = '\"';\nfn f<'a>() { g(\"a\nTell Toby\", '\\'', r#\"say \"Toby\"\"#) }\n#[cfg(test)]\nmod t { \"Toby\" }");
    assert_eq!(lines, [(2, "Tell {user}.\\\"".to_owned()), (4, "a".to_owned()), (5, "Tell Toby".to_owned()), (5, "say \"Toby\"".to_owned())]);
}
