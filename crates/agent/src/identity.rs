//! The assistant's and the user's names in the agent's prompts (Personalise, 26 Oct). The prompt files are written for
//! Heimdall and the user; at every Pi start (`harness`) they are filled with the configured names: whole words in the prose
//! become the new names, while paths, file names, identifiers and backticked examples (the transcript's `User` label)
//! stay, and a short note after the opening paragraph says the transcript labels are internal. With Heimdall and the user
//! the text is unchanged, byte for byte. The host sets the names (`set`) before it starts or reloads Pi.
use std::sync::RwLock;

pub const DEFAULT_ASSISTANT: &str = "Heimdall";
/// The internal label of the user's own lines in every store (transcripts, the archive, the decider's input) and the
/// personal build's default `user_name`. A data label, never a word Heimdall writes about the user: prompts and runtime
/// text name the user through `USER` / `user()` (14 Nov: "it should take the name from the personalization").
pub const DEFAULT_USER: &str = "User";
/// The user in the prompt files and in text Heimdall writes for the agent: the configured name (Personalise, settings
/// `user_name`), or "the user" ("The user" where a sentence or line starts) while none is chosen. Filled by `tokens`,
/// which `fill` (the prompt files) and `template` run, in every edition, where a template is built; never over the
/// user's words, transcripts or tool output, where a literal `{user}` is data.
pub const USER: &str = "{user}";
/// The label the user's own transcript lines carry as the agent reads them (`sent_label`), for prompt examples such as
/// `[17:52:38 Mic · {user_label}] words`.
pub const USER_LABEL: &str = "{user_label}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub assistant: String,
    pub user: String,
}
impl Default for Names {
    fn default() -> Self {
        // The public edition's neutral user until the app sets the configured names (ui::identity::PUBLIC_USER, P-11).
        let user = if cfg!(feature = "public") { PUBLIC_USER } else { DEFAULT_USER };
        Self { assistant: DEFAULT_ASSISTANT.into(), user: user.into() }
    }
}

/// The public edition's user until they choose a name (ui::identity::PUBLIC_USER).
pub const PUBLIC_USER: &str = "User";

/// The names the prompts and transcript labels are written with: nothing to fill in.
fn trained(names: &Names) -> bool {
    names.assistant == DEFAULT_ASSISTANT && names.user == DEFAULT_USER
}

static CURRENT: RwLock<Option<Names>> = RwLock::new(None);

pub fn current() -> Names {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}
/// The names the next Pi start fills in.
pub fn set(names: Names) {
    *CURRENT.write().unwrap() = Some(names);
}

/// The user's name as Heimdall writes it: the configured name, or "the user" while none is chosen (empty, or the public
/// edition's unchosen `User`).
pub fn user_of(names: &Names) -> String {
    let user = names.user.trim();
    if user.is_empty() || user == PUBLIC_USER { "the user".into() } else { user.to_owned() }
}
/// `user_of` the current names.
pub fn user() -> String {
    user_of(&current())
}
/// `user()` where a sentence starts: "The user" while none is chosen.
pub fn user_start() -> String {
    capital(&user())
}
fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}
/// The label of the user's own transcript lines as `for_agent` sends them: `the user` byte for byte for the user's own profile,
/// else the user's name, or `User` while none is chosen.
pub fn sent_label(names: &Names) -> &str {
    if renames_user(names) { public_label(names) } else { DEFAULT_USER }
}

/// `text` with `USER` and `USER_LABEL` filled from `names` (see `USER`). Text without them is returned as is.
pub fn tokens(text: &str, names: &Names) -> String {
    if !text.contains("{user") {
        return text.to_string();
    }
    let text = text.replace(USER_LABEL, sent_label(names));
    let user = user_of(names);
    let mut out = String::with_capacity(text.len() + 16);
    for (i, part) in text.split(USER).enumerate() {
        if i > 0 {
            out.push_str(&if sentence_start(&out) { capital(&user) } else { user.clone() });
        }
        out.push_str(part);
    }
    out
}

/// Whether text written after `before` starts a sentence or a line (a list item, a heading, a quote). A line of
/// hard-wrapped prose is not a start: when the line before is a long one that stops mid-sentence (a letter, a digit or
/// a comma at its end), the text goes on, so "… changes or deletes\n{user}'s things" stays in lower case.
fn sentence_start(before: &str) -> bool {
    let (above, line) = before.rsplit_once('\n').unwrap_or(("", before));
    if line.trim_start_matches(|c: char| c.is_whitespace() || c.is_ascii_digit() || matches!(c, '-' | '*' | '>' | '#' | '.' | '|')).is_empty() {
        let previous = above.rsplit('\n').next().unwrap_or("").trim_end();
        let wrapped = line.chars().all(char::is_whitespace)
            && previous.chars().count() >= WRAPPED_LINE
            && previous.ends_with(|c: char| c.is_alphanumeric() || matches!(c, ',' | ')' | '`' | '\''));
        return !wrapped;
    }
    let t = before.trim_end();
    t.ends_with(['.', '!', '?', ']', ':', '"', '\u{201c}'])
}
/// A line at least this long that stops mid-sentence is taken for hard-wrapped prose (`sentence_start`).
const WRAPPED_LINE: usize = 60;

/// `prompt` with `names` (see the module docs).
pub fn fill(prompt: &str, names: &Names) -> String {
    let prompt = &tokens(prompt, names);
    // Unchanged only for the names the prompt and the transcript labels are written with (Heimdall, the user). The public
    // edition's neutral "User" still gets the note, so the internal `the user` label is never taken for a name (P-11).
    if trained(names) {
        return prompt.to_string();
    }
    let text = replace_names(prompt, &[(DEFAULT_ASSISTANT, &names.assistant), (DEFAULT_USER, &names.user)]);
    // The public edition, and the personal one for a renamed user, send the user's own label (`for_agent`).
    let note = if renames_user(names) {
        let label = public_label(names);
        if label == PUBLIC_USER {
            format!(
                "Your name is {a}; the user has not chosen a name, so call them by name only once they give one. Transcripts \
                 label the user's own lines `{PUBLIC_USER}` and your spoken lines `{DEFAULT_ASSISTANT}`.",
                a = names.assistant
            )
        } else {
            format!("Your name is {a}; the user is {label}. Transcripts label {label}'s own lines `{label}` and your spoken lines `{DEFAULT_ASSISTANT}`.", a = names.assistant)
        }
    } else {
        format!(
            "Your name is {a}; the user is {u}. Transcripts, notices and tool results still label {u}'s own lines `{DEFAULT_USER}` and \
             your spoken lines `{DEFAULT_ASSISTANT}`: internal labels, never names to use.",
            a = names.assistant,
            u = names.user
        )
    };
    // After the opening paragraph (the title and who you are), else first.
    match text.find("\n\n").and_then(|first| text[first + 2..].find("\n\n").map(|second| first + 2 + second)) {
        Some(at) => format!("{}\n\n{note}{}", &text[..at], &text[at..]),
        None => format!("{note}\n\n{text}"),
    }
}

/// `text` with `names` in its prose, without the note (compaction instructions, background prompts).
pub fn names_only(text: &str, names: &Names) -> String {
    if trained(names) {
        return text.to_string();
    }
    replace_names(text, &[(DEFAULT_ASSISTANT, &names.assistant), (DEFAULT_USER, &names.user)])
}

/// The wake instruction's heading as the app writes it (`Bridge::wake…`) and every edition stores it.
pub const WAKE_MARKER: &str = "\nOwner's wake instruction:\n";
/// The same heading as the public edition sends it (`for_agent`): the user, never "the owner" (VM run 5 Nov 05:43).
pub const PUBLIC_WAKE_MARKER: &str = "\nThe user's wake instruction:\n";

/// A wake split at its instruction (the last heading, in either edition's wording): (the data, the instruction).
pub fn split_wake(text: &str) -> Option<(&str, &str)> {
    let (at, len) = [WAKE_MARKER, PUBLIC_WAKE_MARKER].iter().filter_map(|m| text.rfind(m).map(|i| (i, m.len()))).max_by_key(|(i, _)| *i)?;
    Some((&text[..at], &text[at + len..]))
}


fn public_label(names: &Names) -> &str {
    if names.user == DEFAULT_USER || names.user.is_empty() { PUBLIC_USER } else { &names.user }
}

/// What the agent is sent, as the edition says it. Every edition stores `the user` as the label of the user's own lines (the
/// pipeline, the decider, the archive); the personal edition sends it as is, byte for byte, for the user; the public
/// edition never sends the owner's name (VM run 5 Nov 05:43: `[Mic · the user]` and "the user's wake instruction" reached a
/// stranger's agent), and a renamed personal user gets the same wording: `public_wording`.
pub fn for_agent(text: &str, names: &Names) -> String {
    if renames_user(names) { public_wording(text, names) } else { text.to_string() }
}

/// A template Heimdall wrote (a prompt file, a background job's system prompt, a notice's fixed words) with `names`:
/// its `{user}` / `{user_label}` filled (`tokens`), then its prose names (`names_only`). Never for the user's words, a
/// transcript or a tool's output: a literal `{user}` there is data and stays (review 14 Nov).
pub fn template(text: &str, names: &Names) -> String {
    names_only(&tokens(text, names), names)
}

/// Whether `for_agent` rewords: always in the public edition; in the personal one only for a user other than the user, so
/// The user's own profile sends the store's words byte for byte and a renamed user gets the public edition's wording.
fn renames_user(names: &Names) -> bool {
    cfg!(feature = "public") || names.user != DEFAULT_USER
}

/// The public edition's wording of `text`: the wake heading is `PUBLIC_WAKE_MARKER`; a transcript line's label
/// (`[17:52:38 Mic · User] words`) becomes the user's (`public_label`), its spoken words untouched; in any other line
/// (instructions, notices) the stored word User becomes the user's name, or "the user".
pub fn public_wording(text: &str, names: &Names) -> String {
    let label = public_label(names);
    let named = label != PUBLIC_USER;
    let text = text.replace(WAKE_MARKER, PUBLIC_WAKE_MARKER);
    text.split('\n').map(|line| {
        if transcript_line(line) {
            return line.replacen(&format!(" \u{b7} {DEFAULT_USER}]"), &format!(" \u{b7} {label}]"), 1);
        }
        if named { return replace_names(line, &[(DEFAULT_USER, label)]) }
        // "the user", capitalised where a sentence starts; a speaker's label (`13:02 User: words`) is the label.
        let marked = replace_names(line, &[(DEFAULT_USER, "\u{1}")]);
        let mut out = String::with_capacity(marked.len() + 8);
        for (i, part) in marked.split('\u{1}').enumerate() {
            if i > 0 {
                let before = out.trim_end();
                let start = before.is_empty() || before.ends_with(['.', '!', '?', ']', ':']) || before.ends_with("\"");
                out.push_str(if part.starts_with(':') { PUBLIC_USER } else if start { "The user" } else { "the user" });
            }
            out.push_str(part);
        }
        out
    }).collect::<Vec<_>>().join("\n")
}

/// `for_agent` on every `text` and `description` string of a JSON value (MCP tool lists and results).
pub fn for_agent_json(v: &mut serde_json::Value, names: &Names) {
    match v {
        serde_json::Value::Object(map) => for (k, v) in map.iter_mut() {
            match v {
                serde_json::Value::String(s) if k == "text" || k == "description" => *s = for_agent(s, names),
                _ => for_agent_json(v, names),
            }
        },
        serde_json::Value::Array(list) => for v in list { for_agent_json(v, names) },
        _ => {}
    }
}

/// `[17:52:38 Mic · User] words` or `[Mic · User] words`: a labelled line of the transcript.
fn transcript_line(line: &str) -> bool {
    line.starts_with('[') && line.split_once(']').is_some_and(|(head, _)| head.contains(" \u{b7} "))
}

/// Whole-word replacement that leaves paths, file names, identifiers and backticked spans alone (the same rule as
/// `ui::identity::Identity::named`).
fn replace_names(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut code = false;
    let mut rest = text;
    'scan: while let Some(c) = rest.chars().next() {
        if c == '`' {
            code = !code;
        } else if !code {
            for (from, to) in pairs {
                if let Some(after) = rest.strip_prefix(from) {
                    let prev = out.chars().next_back();
                    let mut next = after.chars();
                    let (n1, n2) = (next.next(), next.next());
                    let word_start = prev.is_none_or(|p| !(p.is_alphanumeric() || matches!(p, '_' | '/' | '.' | '-' | '[')));
                    let word_end = n1.is_none_or(|n| !(n.is_alphanumeric() || matches!(n, '_' | '/' | '-')))
                        && !(n1 == Some('.') && n2.is_some_and(char::is_alphanumeric));
                    if word_start && word_end {
                        out.push_str(to);
                        rest = after;
                        continue 'scan;
                    }
                }
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivy() -> Names {
        Names { assistant: "Ivy".into(), user: "Evelyn".into() }
    }

    /// The user's own profile (Heimdall, the user) gets the prompt as it read before the names became `{user}` (14 Nov): every
    /// token is his name, and nothing else changes.
    #[cfg(not(feature = "public"))] // the personal edition's defaults are the trained names
    #[test]
    #[ignore = "needs a stored user name other than the public `User`; this tree stores the public label, so the trained and the neutral names are the same"]
    fn the_default_names_leave_the_prompt_as_written() {
        let filled = fill(crate::harness::PROMPT, &Names::default());
        assert_eq!(filled, crate::harness::PROMPT.replace(USER_LABEL, DEFAULT_USER).replace(USER, DEFAULT_USER));
        assert!(!filled.contains("{user"), "no token is left");
    }

    /// `{user}`: the configured name, or "the user" ("The user" where a sentence or line starts) while none is chosen;
    /// `{user_label}`: the label the agent reads for the user's own lines.
    #[test]
    #[ignore = "needs a stored user name other than the public `User`; this tree stores the public label, so the trained and the neutral names are the same"]
    fn the_user_token_takes_the_configured_name() {
        let unnamed = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        let text = "{user} asked. Tell {user}.\n- {user}'s Mac\n1. {user} again, as `[Mic · {user_label}] hi`";
        assert_eq!(tokens(text, &unnamed), "The user asked. Tell the user.\n- The user's Mac\n1. The user again, as `[Mic · User] hi`");
        // Hard-wrapped prose: a name at the start of a continuation line is mid-sentence; after a full stop, a short
        // line or a blank line it starts one.
        let wrapped = "help them, but ask {user} first before anything a guest wants that reaches outside, changes or deletes\n{user}'s things. Most of what you hear is talk.\n{user} decides.\n\n{user} asked.\nShort line\n{user} again";
        assert_eq!(tokens(wrapped, &unnamed), "help them, but ask the user first before anything a guest wants that reaches outside, changes or deletes\nthe user's things. Most of what you hear is talk.\nThe user decides.\n\nThe user asked.\nShort line\nThe user again");
        assert_eq!(tokens(text, &ivy()), "Evelyn asked. Tell Evelyn.\n- Evelyn's Mac\n1. Evelyn again, as `[Mic · Evelyn] hi`");
        let empty = Names { assistant: DEFAULT_ASSISTANT.into(), user: String::new() };
        assert_eq!(user_of(&empty), "the user");
        if !cfg!(feature = "public") {
            let owner = Names::default();
            assert_eq!(tokens(text, &owner), "User asked. Tell User.\n- User's Mac\n1. User again, as `[Mic · User] hi`");
        }
        // The template fillers run it, in every edition.
        for f in [fill, template] {
            assert!(!f("# H\n\nFor {user}.\n\nRules.", &ivy()).contains("{user"));
        }
    }

    /// Review 14 Nov: the user's own words, transcripts and tool output are data. A literal `{user}` in them (the user
    /// typing it, a thread read of Heimdall's own code) reaches the agent unchanged, in every edition and for every name.
    #[test]
    fn a_literal_user_token_in_words_or_tool_output_passes_unchanged() {
        let neutral = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        for names in [Names::default(), ivy(), neutral] {
            let typed = "[Heimdall typed message]\nwhy does the prompt say {user} and {user_label}?";
            assert!(for_agent(typed, &names).contains("say {user} and {user_label}?"), "{names:?}");
            assert!(names_only(typed, &names).contains("say {user} and {user_label}?"), "{names:?}");
            let mut result = serde_json::json!({"content":[{"type":"text","text":"crates/agent/src/identity.rs: pub const USER: &str = \"{user}\";"}]});
            for_agent_json(&mut result, &names);
            assert_eq!(result["content"][0]["text"], "crates/agent/src/identity.rs: pub const USER: &str = \"{user}\";", "{names:?}");
        }
    }

    /// P-11: the public default user is "User", and the prompt says the name is not chosen yet and how the transcript
    /// labels the user (`User`, never the owner's name: VM run 5 Nov 05:43; the personal edition words a renamed user
    /// the same way since 14 Nov); the trained names still leave the prompt untouched.
    #[test]
    #[ignore = "needs a stored user name other than the public `User`; this tree stores the public label, so the trained and the neutral names are the same"]
    fn the_public_neutral_user_gets_the_label_note_and_the_trained_names_do_not() {
        let neutral = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        let text = fill("# Heimdall\n\nYou are Heimdall for User.\n\nRules.", &neutral);
        assert!(text.contains("the user has not chosen a name") && text.contains("`User`"), "{text}");
        assert!(text.contains("You are Heimdall for User."), "{text}");
        let trained = Names { assistant: DEFAULT_ASSISTANT.into(), user: DEFAULT_USER.into() };
        if cfg!(feature = "public") {
            assert_eq!(Names::default(), neutral);
        } else {
            assert_eq!(fill("You are Heimdall for User.", &trained), "You are Heimdall for User.");
            assert_eq!(Names::default(), trained);
        }
    }

    #[cfg(not(feature = "public"))] // asserts the personal prompt's own opening and wording
    #[test]
    #[ignore = "pins the personal-edition prompt, which is not in the public repository"]
    fn other_names_fill_the_prose_and_keep_labels_paths_and_tools() {
        let p = fill(crate::harness::PROMPT, &ivy());
        let opening: String = p.lines().take(8).collect::<Vec<_>>().join("\n");
        assert!(opening.starts_with("# Ivy\n\nYou are Ivy, Evelyn's assistant"), "{opening}");
        assert!(p.contains("Your name is Ivy; the user is Evelyn. Transcripts label Evelyn's own lines `Evelyn`"), "{opening}");
        // What stays: tool names, paths, the delivery marker, backticked transcript examples.
        assert!(p.contains("`mcp__heimdall__<name>`"));
        assert!(p.contains("Mic · Evelyn]"), "transcript label examples show the label the agent reads (`sent_label`)");
        for kept in ["Projects/Heimdall", "Heimdall.app"] {
            assert_eq!(p.contains(kept), crate::harness::PROMPT.contains(kept), "{kept}");
        }
        // No prose "Heimdall" or "the user" is left outside code and paths.
        let prose: String = p.split('`').step_by(2).collect::<Vec<_>>().join(" ");
        for word in prose.split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '['))) {
            let bare = word.trim_end_matches('.');
            assert!(bare != "Heimdall" && bare != "User", "left in prose: {word}");
        }
        // A user who chose no name is "the user" (the public edition's `User`).
        let unnamed = fill(crate::harness::PROMPT, &Names { assistant: "Ivy".into(), user: PUBLIC_USER.into() });
        assert!(unnamed.starts_with("# Ivy\n\nYou are Ivy, the user's assistant"), "{}", &unnamed[..200]);
    }

    /// Cleanup night 14 Nov (DOC-17): the task preamble gets the configured names like Heimdall's prompt; the user's own
    /// profile sends it byte for byte, and the worker's backticked marks and paths keep their words.
    #[cfg(not(feature = "public"))]
    #[test]
    #[ignore = "needs a stored user name other than the public `User`; this tree stores the public label, so the trained and the neutral names are the same"]
    fn the_task_preamble_takes_the_configured_names() {
        let text = crate::harness::WORKER_PROMPT;
        assert_eq!(template(text, &Names::default()), text.replace(USER, DEFAULT_USER));
        let prose: String = template(text, &ivy()).split('`').step_by(2).collect::<Vec<_>>().join(" ");
        assert!(no_owner(&prose), "{}", prose.lines().find(|l| !no_owner(l)).unwrap_or_default());
        let named = template(crate::harness::WORKER_PROMPT, &ivy());
        assert!(named.starts_with("# Ivy task\n\nIvy, Evelyn's voice assistant"), "{}", &named[..80]);
        assert!(named.contains("`[From Heimdall, the orchestrator that started this task, speaking for Evelyn]`"), "the mark names the user as the T3 layer does (`t3::workers::steer_mark`)");
    }

    #[test]
    fn replacement_is_whole_words_only() {
        let n = ivy();
        assert!(fill("a\n\nb", &n).starts_with("Your name is Ivy;"), "a prompt without an opening paragraph gets the note first");
        let r = |t: &str| replace_names(t, &[(DEFAULT_ASSISTANT, &n.assistant), (DEFAULT_USER, &n.user)]);
        assert_eq!(r("Heimdall's tools, User's Mac; TobyNoSkillSon; heimdall; [Heimdall delivery: d]"), "Ivy's tools, Evelyn's Mac; TobyNoSkillSon; heimdall; [Heimdall delivery: d]");
        assert_eq!(r("~/Applications/Heimdall.app and Projects/Heimdall, Heimdall."), "~/Applications/Heimdall.app and Projects/Heimdall, Ivy.");
    }

    /// No stored `User` label left as a word, backticks included.
    fn no_owner(text: &str) -> bool {
        !text.split(|c: char| !c.is_alphanumeric()).any(|w| w == DEFAULT_USER)
    }

    /// VM run 5 Nov 05:43: a public agent read the stored label and the owner's wake heading although Setup saved the
    /// user's name. The public wording gives the user's label and name, or "the user", in labels, the heading,
    /// instructions and notices; spoken words stay.
    #[test]
    fn the_public_wording_never_names_the_owner() {
        let wake = format!("[Heimdall delivery: d1]\nTranscript:\n[17:52:38 Mic \u{b7} User] Heimdall, what time is it?\n[17:52:40 Discord \u{b7} Dave] ask User\n{WAKE_MARKER}Act on User's latest request to you in the transcript, if there is one. Everyone else is context.");
        let neutral = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        let out = public_wording(&wake, &neutral);
        assert!(out.contains("[17:52:38 Mic \u{b7} User] Heimdall, what time is it?") && out.contains(PUBLIC_WAKE_MARKER) && !out.contains(WAKE_MARKER), "{out}");
        assert!(out.ends_with("Act on the user's latest request to you in the transcript, if there is one. Everyone else is context."), "{out}");
        assert!(out.contains("[17:52:40 Discord \u{b7} Dave] ask User"), "spoken words are data, never reworded");
        let tester = Names { assistant: DEFAULT_ASSISTANT.into(), user: "Tester".into() };
        let out = public_wording(&wake, &tester);
        assert!(out.contains("Mic \u{b7} Tester]") && out.contains("Act on Tester's latest request"), "{out}");
        assert!(out.contains("[17:52:40 Discord \u{b7} Dave] ask User"), "a guest's words keep what they said");
        assert_eq!(public_wording("Question ask-1: User clicked Yes. Question: Clear it?", &neutral), "Question ask-1: The user clicked Yes. Question: Clear it?");
        assert_eq!(public_wording("Question ask-1: User clicked Yes. Question: Clear it?", &tester), "Question ask-1: Tester clicked Yes. Question: Clear it?");
        assert_eq!(public_wording("13:02 User: the CAD model", &neutral), "13:02 User: the CAD model", "a speaker's label");
        assert_eq!(public_wording("13:02 User: the CAD model", &tester), "13:02 Tester: the CAD model", "a named speaker's label");
        assert_eq!(public_wording("[Heimdall notice: x]\nUser talked over you; he heard only the start of: hi", &neutral), "[Heimdall notice: x]\nThe user talked over you; he heard only the start of: hi");
        assert_eq!(public_wording("User-facing `User` text in src/User.rs", &tester), "User-facing `User` text in src/User.rs", "words, code and paths that only contain the label stay");
        assert_eq!(split_wake(&public_wording(&wake, &neutral)).map(|(_, i)| i.starts_with("Act on the user's")), Some(true), "parsers find either heading");
        for m in [WAKE_MARKER, PUBLIC_WAKE_MARKER] { assert!(split_wake(&format!("data{m}do it")).is_some_and(|(d, i)| d == "data" && i == "do it")); }
        // MCP results and tool descriptions: reworded for any user but the user, in either edition (14 Nov).
        let mut v = serde_json::json!({"content":[{"type":"text","text":"[Mic \u{b7} User] hi"}],"tools":[{"name":"x","description":"User's files"}]});
        for_agent_json(&mut v, &neutral);
        // This tree's stored label is the public edition's own `User`, so the neutral user's labels keep it; the
        // renamed-user cases below hold the rewording (the personal tree checks the stored name is gone here).
        assert!(v.to_string().contains("[Mic \u{b7} User] hi") || v.to_string().contains("[Mic · User] hi"), "{v}");
    }

    /// The personal edition sends the store's own words, byte for byte.
    #[cfg(not(feature = "public"))]
    #[test]
    fn the_personal_edition_sends_the_stored_label_as_written() {
        let wake = format!("[Mic \u{b7} User] hi{WAKE_MARKER}Act on User's latest request.");
        assert_eq!(for_agent(&wake, &current()), wake);
    }

    /// Cleanup night 14 Nov (DOC-22): the agent-bound strings the public edition rewords go through the same wording
    /// in the personal edition for a renamed user; the user's own profile (a renamed assistant included) stays byte for
    /// byte.
    #[cfg(not(feature = "public"))]
    #[test]
    #[ignore = "needs a stored user name other than the public `User`; this tree stores the public label, so the trained and the neutral names are the same"]
    fn the_personal_edition_words_a_renamed_user_like_the_public_one() {
        let wake = format!("[17:52:38 Mic \u{b7} User] hi\n[Heimdall notice: x]\nOwner talked over you.{WAKE_MARKER}Act on User's latest request.");
        let tool = serde_json::json!({"content":[{"type":"text","text":"Sent to User's phone"}],"tools":[{"name":"x","description":"Search User's Mac"}]});
        for owner in [Names::default(), Names { assistant: "Ivy".into(), user: DEFAULT_USER.into() }] {
            assert_eq!(for_agent(&wake, &owner), wake, "{owner:?}");
            let mut v = tool.clone();
            for_agent_json(&mut v, &owner);
            assert_eq!(v, tool, "{owner:?}");
        }
        let evelyn = ivy();
        let out = for_agent(&wake, &evelyn);
        assert_eq!(out, public_wording(&wake, &evelyn));
        assert!(out.contains("[17:52:38 Mic \u{b7} Evelyn] hi") && out.contains("\nEvelyn talked over you.") && out.ends_with("Act on Evelyn's latest request."), "{out}");
        assert_eq!(split_wake(&out).map(|(_, i)| i), Some("Act on Evelyn's latest request."));
        let mut v = tool.clone();
        for_agent_json(&mut v, &evelyn);
        assert!(no_owner(&v.to_string()) && v.to_string().contains("Search Evelyn's Mac"), "{v}");
        let unnamed = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        assert!(for_agent(&wake, &unnamed).ends_with("Act on the user's latest request."));
        let note = fill("# Heimdall\n\nYou are Heimdall for User.\n\nRules.", &evelyn);
        assert!(note.contains("Transcripts label Evelyn's own lines `Evelyn`"), "{note}");
    }

    /// The public edition's system prompt and what it sends carry the user's chosen name, never the stored label or the
    /// owner's heading (VM run 5 Nov 05:43); before a name is chosen, the label is `User` and the words "the user".
    #[cfg(feature = "public")]
    #[test]
    fn a_public_prompt_and_its_labels_never_say_owner() {
        for names in [Names::default(), Names { assistant: "Ivy".into(), user: "Tester".into() }] {
            let prompt = fill(crate::harness::PROMPT, &names);
            assert!(no_owner(&prompt), "{names:?}: {}", prompt.lines().find(|l| !no_owner(l)).unwrap_or_default());
            let sent = for_agent(&format!("[17:52:38 Mic \u{b7} User] hi{WAKE_MARKER}Act on User's latest request."), &names);
            assert!(no_owner(&sent), "{sent}");
        }
    }
}
