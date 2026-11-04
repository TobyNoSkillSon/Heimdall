//! The page a task makes, read from its brief: "when `delegate_task`'s brief names an output
//! `.html` path, bind it as the island's `open`". `output_page` finds absolute, `~/` and `file://` paths ending in
//! `.html` or `.htm` and judges each by the nearest cue word before it on its line ("write", "output", "save to" make
//! it the output; "read", "from", "like", "template" make it an input). The first output wins; with no cue at all the
//! first uncued path that does not exist yet stands in (a page already there is more likely something to read); an
//! input is never bound. Paths with spaces are not found (`file://` ones may encode them).

/// Verbs and labels that settle a path's role on their own.
const OUTPUT: [&str; 33] = [
    "output", "outputs", "write", "writes", "writing", "written", "save", "saves", "saved", "saving", "produce", "produces",
    "producing", "build", "builds", "create", "creates", "generate", "generates", "render", "renders", "publish", "export",
    "exports", "put", "deliver", "deliverable", "update", "updates", "fix", "rebuild", "redo", "make",
];
const INPUT: [&str; 29] = [
    "read", "reads", "reading", "from", "input", "inputs", "template", "reference", "references", "based", "see", "like",
    "source", "sources", "compare", "mirror", "following", "inspired", "look", "match", "review", "same", "similar",
    "summarise", "summarize", "summary", "link", "links", "style",
];
/// Words that lean to the output but yield to a settling word just before them ("read the report /a.html").
const WEAK: [&str; 8] = ["to", "into", "report", "page", "dashboard", "result", "results", "final"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Role { Output, Input, Unknown }

/// The output page `brief` names, absolute (`~/` expanded), or `None`.
pub fn output_page(brief: &str) -> Option<String> {
    let found = paths(brief);
    let mut uncued = None;
    let mut prev_end = 0;
    for (start, end) in found {
        let line_start = brief[..start].rfind('\n').map_or(0, |i| i + 1);
        let role = role(&brief[line_start.max(prev_end)..start]);
        prev_end = end;
        let path = absolute(&brief[start..end]);
        match role {
            Role::Output => return Some(path),
            Role::Unknown if uncued.is_none() && !std::path::Path::new(&path).exists() => uncued = Some(path),
            _ => {}
        }
    }
    uncued
}

/// The role the words before a path give it: the nearest cue decides; a weak one yields to a settling word within
/// the three words before it.
fn role(before: &str) -> Role {
    let lower = before.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let settle = |w: &str| if OUTPUT.contains(&w) { Some(Role::Output) } else if INPUT.contains(&w) { Some(Role::Input) } else { None };
    for (k, w) in words.iter().enumerate().rev() {
        if let Some(r) = settle(w) { return r }
        if WEAK.contains(w) {
            return words[k.saturating_sub(3)..k].iter().rev().find_map(|w| settle(w)).unwrap_or(Role::Output);
        }
    }
    Role::Unknown
}

/// Byte ranges of `.html`/`.htm` paths: `/…`, `~/…` or `file://…`, starting after a space or an opening mark and
/// ending at a space or a closing mark (a sentence's last `.`, `:`, `!` or `?` is not part of it).
fn paths(s: &str) -> Vec<(usize, usize)> {
    let stop = |c: char| c.is_whitespace() || "`\"'<>()[]{},;|*".contains(c);
    let opens = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || "`\"'<>([{,;*=".contains(c));
    let mut out = vec![];
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let at_edge = opens(s[..i].chars().next_back());
        let lead = if !at_edge { None } else if rest.starts_with("file:///") { Some(7) } else if rest.starts_with("~/") || rest.starts_with('/') && !rest.starts_with("//") { Some(0) } else { None };
        let Some(skip) = lead else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let len = rest.find(stop).unwrap_or(rest.len());
        let token = rest[..len].trim_end_matches(['.', ':', '!', '?']);
        let lower = token.to_lowercase();
        let name = lower.rsplit('/').next().unwrap_or("");
        if (name.ends_with(".html") && name.len() > 5) || (name.ends_with(".htm") && name.len() > 4) {
            out.push((i + skip, i + token.len()));
        }
        i += len.max(1);
    }
    out
}

/// `~/` expanded; `%xx` decoded (only `file://` paths carry them, and a real `%` in a path is rare).
fn absolute(p: &str) -> String {
    let p = match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default().trim_end_matches('/')),
        None => p.to_string(),
    };
    if !p.contains('%') { return p }
    let b = p.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len()).then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        match hex {
            Some(v) => { out.push(v); i += 3 }
            None => { out.push(b[i]); i += 1 }
        }
    }
    String::from_utf8(out).unwrap_or(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(p: &str) -> String { absolute(&format!("~/{p}")) }

    #[test]
    fn the_output_page_is_the_one_the_brief_writes() {
        let b = "Read /Users/t/Notes/Projects/X/style.html for the look, then write the comparison to ~/Notes/Research/voices.html.";
        assert_eq!(output_page(b).as_deref(), Some(home("Notes/Research/voices.html").as_str()));
        assert_eq!(output_page("Output: `/tmp/a/report.html`").as_deref(), Some("/tmp/a/report.html"));
        assert_eq!(output_page("- Report: /tmp/r.htm\n- Notes: /tmp/n.md").as_deref(), Some("/tmp/r.htm"));
        assert_eq!(output_page("Save the page as (/tmp/out.html)!").as_deref(), Some("/tmp/out.html"));
        assert_eq!(output_page("Build it at file:///tmp/f.html").as_deref(), Some("/tmp/f.html"));
        assert_eq!(output_page("Write file:///tmp/My%20Report.html").as_deref(), Some("/tmp/My Report.html"));
        // Several outputs: the first.
        assert_eq!(output_page("Write /tmp/one.html and then /tmp/two.html").as_deref(), Some("/tmp/one.html"));
    }

    #[test]
    fn inputs_are_never_bound_and_an_uncued_path_stands_in() {
        assert_eq!(output_page("Use the layout from /tmp/old.html. Look at the numbers."), None);
        assert_eq!(output_page("Read the report /tmp/old.html and summarise it in chat."), None);
        assert_eq!(output_page("Make it look like /tmp/bench.html"), None);
        for b in ["Match the style of /tmp/old.html", "Review /tmp/old.html", "Make it similar to /tmp/old.html", "Summarise the report /tmp/old.html"] {
            assert_eq!(output_page(b), None, "{b}");
        }
        // An uncued page that is already there is more likely an input.
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("old.html");
        std::fs::write(&old, "<h1>old</h1>").unwrap();
        assert_eq!(output_page(&format!("User liked {}", old.display())), None);
        assert_eq!(output_page("Template: /tmp/t.html\nWrite the page to /tmp/new.html"), Some("/tmp/new.html".into()));
        assert_eq!(output_page("User wants /tmp/costs.html to show the totals first."), Some("/tmp/costs.html".into()));
        // An input first, an uncued page later: the uncued one.
        assert_eq!(output_page("Read /tmp/in.html.\n/tmp/costs.html needs the totals."), Some("/tmp/costs.html".into()));
    }

    #[test]
    fn only_html_paths_count() {
        for b in [
            "Write the notes to /tmp/notes.md",
            "Open https://example.com/page.html",
            "Write /tmp/report.html.bak",
            "Write /tmp/a.html/index",
            "Write relative/report.html",
            "Write /tmp/.html",
            "no paths here",
            "",
        ] {
            assert_eq!(output_page(b), None, "{b}");
        }
        assert_eq!(output_page("WRITE /TMP/R.HTML"), Some("/TMP/R.HTML".into()));
    }
}
