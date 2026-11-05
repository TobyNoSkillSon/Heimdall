//! The assistant's and the user's names in the agent's prompts (Personalise, 26 Oct). The prompt files are written for
//! Heimdall and the user; at every Pi start (`harness`) they are filled with the configured names: whole words in the prose
//! become the new names, while paths, file names, identifiers and backticked examples (the transcript's `User` label)
//! stay, and a short note after the opening paragraph says the transcript labels are internal. With Heimdall and the user
//! the text is unchanged, byte for byte. The host sets the names (`set`) before it starts or reloads Pi.
use std::sync::RwLock;

pub const DEFAULT_ASSISTANT: &str = "Heimdall";
pub const DEFAULT_USER: &str = "User";

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

/// `prompt` with `names` (see the module docs).
pub fn fill(prompt: &str, names: &Names) -> String {
    // Unchanged only for the names the prompt and the transcript labels are written with (Heimdall, the user). The public
    // edition's neutral "User" still gets the note, so the internal `User` label is never taken for a name (P-11).
    if trained(names) && !cfg!(feature = "public") {
        return prompt.to_string();
    }
    let text = replace_names(prompt, &[(DEFAULT_ASSISTANT, &names.assistant), (DEFAULT_USER, &names.user)]);
    // The public edition sends the user's own label (`for_agent`), never the owner's name.
    let note = if cfg!(feature = "public") {
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
    } else if names.user == PUBLIC_USER {
        format!(
            "Your name is {a}; the user has not chosen a name, so call them by name only once they give one. Transcripts, \
             notices and tool results label the user's own lines `{DEFAULT_USER}` and your spoken lines `{DEFAULT_ASSISTANT}`: \
             internal labels, never names to use.",
            a = names.assistant
        )
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

/// The label of the user's own lines as the agent reads them: the stored `User` in the personal edition (the decider and
/// the prompts were written with it); the user's name in the public one, `User` until they choose one.
pub fn user_label(names: &Names) -> String {
    if !cfg!(feature = "public") { return DEFAULT_USER.into(); }
    public_label(names).into()
}

fn public_label(names: &Names) -> &str {
    if names.user == DEFAULT_USER || names.user.is_empty() { PUBLIC_USER } else { &names.user }
}

/// What the agent is sent, as the edition says it. Every edition stores `User` as the label of the user's own lines (the
/// pipeline, the decider, the archive); the personal edition sends it as is, byte for byte, and the public edition
/// sends the user's chosen name, or "the user" (VM run 5 Nov 05:43: the stored label and the owner's wake heading
/// reached an agent whose user had saved a name in Setup): `public_wording`.
pub fn for_agent(text: &str, names: &Names) -> String {
    if cfg!(feature = "public") { public_wording(text, names) } else { text.to_string() }
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
    if !cfg!(feature = "public") { return; }
    match v {
        serde_json::Value::Object(map) => for (k, v) in map.iter_mut() {
            match v {
                serde_json::Value::String(s) if k == "text" || k == "description" => *s = public_wording(s, names),
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

    #[cfg(not(feature = "public"))] // the personal edition's defaults are the trained names
    #[test]
    fn the_default_names_leave_the_prompt_as_written() {
        assert_eq!(fill(crate::harness::PROMPT, &Names::default()), crate::harness::PROMPT);
    }

    /// P-11: the public default user is "User", and the prompt says the name is not chosen yet and how the transcript
    /// labels the user (`User` in both editions until a name is chosen); the trained names still leave the personal
    /// prompt untouched.
    #[test]
    fn the_public_neutral_user_gets_the_label_note_and_the_trained_names_do_not() {
        let neutral = Names { assistant: DEFAULT_ASSISTANT.into(), user: PUBLIC_USER.into() };
        if cfg!(feature = "public") {
            let text = fill("# Heimdall\n\nYou are Heimdall for User.\n\nRules.", &neutral);
            assert!(text.contains("the user has not chosen a name") && text.contains("`User`"), "{text}");
            assert!(text.contains("You are Heimdall for User."), "{text}");
        }
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
        assert!(p.contains("Your name is Ivy; the user is Evelyn. Transcripts, notices and tool results still label Evelyn's own lines `User`"), "{opening}");
        // What stays: tool names, paths, the delivery marker, backticked transcript examples.
        assert!(p.contains("`mcp__heimdall__<name>`"));
        assert!(p.contains("Mic · User]"), "transcript label examples keep the internal label");
        for kept in ["Projects/Heimdall", "Heimdall.app"] {
            assert_eq!(p.contains(kept), crate::harness::PROMPT.contains(kept), "{kept}");
        }
        // No prose "Heimdall" or "the user" is left outside code and paths.
        let prose: String = p.split('`').step_by(2).collect::<Vec<_>>().join(" ");
        for word in prose.split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '['))) {
            let bare = word.trim_end_matches('.');
            assert!(bare != "Heimdall" && bare != "User", "left in prose: {word}");
        }
        // The public prompt too.
        let public = fill(include_str!("../prompt/public/heimdall.md"), &ivy());
        assert!(public.starts_with("# Ivy\n\nYou are Ivy, the user's assistant"), "{}", &public[..200]);
    }

    #[test]
    fn replacement_is_whole_words_only() {
        let n = ivy();
        assert!(fill("a\n\nb", &n).starts_with("Your name is Ivy;"), "a prompt without an opening paragraph gets the note first");
        let r = |t: &str| replace_names(t, &[(DEFAULT_ASSISTANT, &n.assistant), (DEFAULT_USER, &n.user)]);
        assert_eq!(r("Heimdall's tools, User's Mac; TobyNoSkillSon; heimdall; [Heimdall delivery: d]"), "Ivy's tools, Evelyn's Mac; TobyNoSkillSon; heimdall; [Heimdall delivery: d]");
        assert_eq!(r("~/Applications/Heimdall.app and Projects/Heimdall, Heimdall."), "~/Applications/Heimdall.app and Projects/Heimdall, Ivy.");
    }

    /// No stored `User` label or owner's wake heading left (`TobyNoSkillSon`, the developer's GitHub account, is
    /// another word): what a named user's public agent reads.
    #[cfg(feature = "public")]
    fn names_the_user(text: &str, name: &str) -> bool {
        !text.split(|c: char| !c.is_alphanumeric()).any(|w| w == DEFAULT_USER) && !text.contains(WAKE_MARKER) && text.contains(name)
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
        // MCP results and tool descriptions, public edition only.
        let mut v = serde_json::json!({"content":[{"type":"text","text":"[Mic \u{b7} User] hi"}],"tools":[{"name":"x","description":"User's files"}]});
        for_agent_json(&mut v, &tester);
        let text = v.to_string();
        assert_eq!(text.contains("Tester"), cfg!(feature = "public"), "{text}");
        assert_eq!(text.contains("User"), !cfg!(feature = "public"), "{text}");
    }

    /// The personal edition sends the store's own words, byte for byte.
    #[cfg(not(feature = "public"))]
    #[test]
    fn the_personal_edition_sends_the_stored_label_as_written() {
        let wake = format!("[Mic \u{b7} User] hi{WAKE_MARKER}Act on User's latest request.");
        assert_eq!(for_agent(&wake, &current()), wake);
        assert_eq!(user_label(&current()), DEFAULT_USER);
    }

    /// The public edition's system prompt and what it sends carry the user's chosen name, never the stored label or the
    /// owner's heading (VM run 5 Nov 05:43); before a name is chosen, the label is `User` and the words "the user".
    #[cfg(feature = "public")]
    #[test]
    fn a_public_prompt_and_its_labels_carry_the_users_name() {
        let tester = Names { assistant: "Ivy".into(), user: "Tester".into() };
        let sent = for_agent(&format!("[17:52:38 Mic \u{b7} User] hi{WAKE_MARKER}Act on User's latest request."), &tester);
        assert!(names_the_user(&sent, "Tester"), "{sent}");
        assert_eq!(user_label(&tester), "Tester");
        let prompt = fill(crate::harness::PROMPT, &tester);
        assert!(prompt.contains("the user is Tester") && prompt.contains("`Tester`"), "{}", &prompt[..prompt.len().min(600)]);
        let neutral = Names::default();
        let sent = for_agent(&format!("[17:52:38 Mic \u{b7} User] hi{WAKE_MARKER}Act on User's latest request."), &neutral);
        assert!(sent.contains("[17:52:38 Mic \u{b7} User] hi") && sent.ends_with("Act on the user's latest request.") && !sent.contains(WAKE_MARKER), "{sent}");
        assert_eq!(user_label(&neutral), PUBLIC_USER);
        assert!(fill(crate::harness::PROMPT, &neutral).contains("the user has not chosen a name"));
    }
}
