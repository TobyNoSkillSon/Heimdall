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
    let note = if names.user == PUBLIC_USER {
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

    /// P-11: the public default user is "User", and the prompt says the name is not chosen yet and that `User` is
    /// only the transcript label; the trained names still leave the prompt untouched.
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
}
