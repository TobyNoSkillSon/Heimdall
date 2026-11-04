//! The names the decider reads ("we artificially attach 'your name is Heimdall, the user's name is
//! The user', and then we can swap it: your name is Ivy, the user's name is Evelyn"). The host labels its own lines
//! `User`, `the user (typed)` and `Heimdall` whatever the user chose; the decider's context and questions say the configured
//! names instead, after a header that names both, and the wake spotter (`send::explicit_trigger`, `addressed_by_name`)
//! listens for the assistant's name (no separate wake words; "Ivy, hello" wakes Ivy).
//!
//! Heimdall and the user are the names decider v4 was trained on: then the context, the
//! questions and the spotter are byte for byte what they were before names existed (no header). Decider v5 is to be
//! trained with the header on every example and the names swapped (docs/decider-v5-names.md); a v5+ model gets the
//! header with the default names too (`header_always`).
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub const DEFAULT_ASSISTANT: &str = "Heimdall";
pub const DEFAULT_USER: &str = "User";
/// The host's own labels for the user's and the assistant's lines (the transcript's speaker names).
pub const USER_LABEL: &str = "User";
pub const TYPED_LABEL: &str = "User (typed)";
pub const ASSISTANT_LABEL: &str = "Heimdall";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub assistant: String,
    pub user: String,
    /// The name's words, lower case ("Ivy" → [["ivy"]]), and a several-word name joined as one word too.
    pub wake: Vec<Vec<String>>,
}
impl Default for Names {
    fn default() -> Self {
        Self { assistant: DEFAULT_ASSISTANT.into(), user: DEFAULT_USER.into(), wake: vec![vec!["heimdall".into()]] }
    }
}
impl Names {
    /// The wake is the assistant's name; the spotter adds the greetings and, for Heimdall, its observed misspellings.
    pub fn new(assistant: &str, user: &str) -> Self {
        // A name that opens with a greeting ("Hi Ivy") wakes on the words after it, as `ui::identity::name_tokens` reads it.
        let mut name = words(assistant);
        loop {
            let lead = match name.as_slice() {
                [w, u, _, ..] if w == "wake" && u == "up" => 2,
                [g, _, ..] if GREETINGS.contains(&g.as_str()) => 1,
                _ => break,
            };
            name.drain(..lead);
        }
        let mut wake = vec![name];
        // "Mary-Jane" or "D'Arcy" as one token too: guests' lines are compared with hyphens and apostrophes dropped.
        if wake[0].len() > 1 {
            wake.push(vec![wake[0].concat()]);
        }
        wake.retain(|w| !w.is_empty());
        Self { assistant: assistant.into(), user: user.into(), wake }
    }
    /// The user's names: the spotter keeps its observed misspellings of "Heimdall".
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    /// Heimdall and the user: no header, no relabelling.
    pub fn default_names(&self) -> bool {
        self.assistant == DEFAULT_ASSISTANT && self.user == DEFAULT_USER
    }
    /// "Assistant name: Ivy. User name: Evelyn." — the first line of the decider's state.
    pub fn header(&self) -> String {
        format!("Assistant name: {}. User name: {}.", self.assistant, self.user)
    }
    /// A host speaker label as the decider reads it.
    pub fn label<'a>(&'a self, speaker: &'a str) -> std::borrow::Cow<'a, str> {
        if self.default_names() {
            return speaker.into();
        }
        match speaker {
            USER_LABEL => self.user.as_str().into(),
            TYPED_LABEL => format!("{} (typed)", self.user).into(),
            ASSISTANT_LABEL => self.assistant.as_str().into(),
            other => other.into(),
        }
    }
    /// A question's text with these names in place of Heimdall and the user (whole words; option keys stay).
    pub fn question(&self, question: &Value) -> Value {
        if self.default_names() {
            return question.clone();
        }
        match question {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|v| self.question(v)).collect()),
            Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), self.question(v))).collect()),
            other => other.clone(),
        }
    }
    /// Plain text with these names in place of Heimdall and the user (whole words).
    pub fn text(&self, s: &str) -> String {
        if self.default_names() {
            return s.into();
        }
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        'scan: while let Some(c) = rest.chars().next() {
            for (from, to) in [(DEFAULT_ASSISTANT, self.assistant.as_str()), (DEFAULT_USER, self.user.as_str())] {
                if rest.starts_with(from) {
                    let before = out.chars().next_back();
                    let after = rest[from.len()..].chars().next();
                    if !before.is_some_and(|b| b.is_alphanumeric() || b == '_') && !after.is_some_and(|a| a.is_alphanumeric() || a == '_') {
                        out.push_str(to);
                        rest = &rest[from.len()..];
                        continue 'scan;
                    }
                }
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }
    /// Whether `token` (lower case, letters and digits) is one of the single-word wake names.
    pub fn exact(&self, token: &str) -> bool {
        self.wake.iter().any(|w| w.len() == 1 && w[0] == token)
    }
    /// Whether the wake word `w` starts at `words[i]`.
    pub fn phrase_at(&self, words: &[String], i: usize) -> Option<usize> {
        self.wake.iter().filter(|w| w.len() > 1).find(|w| words.len() >= i + w.len() && words[i..i + w.len()] == w[..]).map(Vec::len)
    }
    /// Whether any wake word occurs in `words`.
    pub fn mentioned(&self, words: &[String]) -> bool {
        (0..words.len()).any(|i| self.exact(&words[i]) || self.phrase_at(words, i).is_some())
    }
}

/// Greetings that may lead a name: `ui::identity::GREETINGS` ("wake up" is matched as its two words).
const GREETINGS: [&str; 6] = ["hey", "hi", "hello", "ok", "okay", "yo"];

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

static CURRENT: RwLock<Option<Arc<Names>>> = RwLock::new(None);

/// The names in use; Heimdall and the user until the host sets others.
pub fn current() -> Arc<Names> {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}
/// The host's names (`crates/app` identity), from the next decision on.
pub fn set(names: Names) {
    *CURRENT.write().unwrap() = Some(Arc::new(names));
}

/// Whether the shipped decider was trained with the names header on every example: v5 and later
/// (docs/decider-v5-names.md). Earlier models see the header only when the names are not Heimdall and the user. The v5
/// request itself is `send::Snapshot::fit_v5` (header always, questions never renamed).
pub fn header_always() -> bool {
    crate::send_v3::version(std::path::Path::new(crate::d1::DEFAULT_MODEL)).is_some_and(|v| v >= 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivy() -> Names {
        Names::new("Ivy", "Evelyn")
    }

    #[test]
    fn defaults_change_nothing() {
        let d = Names::default();
        assert!(d.is_default() && d.default_names());
        assert_eq!(Names::new("Heimdall", "User"), d);
        assert_eq!(d.label("User (typed)"), "User (typed)");
        let q = crate::send::question();
        assert_eq!(d.question(&q), q);
        assert_eq!(d.text("Did User ask Heimdall?"), "Did User ask Heimdall?");
    }

    #[test]
    fn other_names_relabel_and_rewrite_questions() {
        let n = ivy();
        assert_eq!(n.wake, [vec!["ivy".to_string()]]);
        assert_eq!(n.header(), "Assistant name: Ivy. User name: Evelyn.");
        assert_eq!((n.label("User"), n.label("User (typed)"), n.label("Heimdall"), n.label("Dave")), ("Evelyn".into(), "Evelyn (typed)".into(), "Ivy".into(), "Dave".into()));
        assert_eq!(n.text("Did User, right now, ask Heimdall to look? not_for_heimdall Ownerx"), "Did Evelyn, right now, ask Ivy to look? not_for_heimdall Ownerx");
        let q = n.question(&crate::send::question());
        assert!(!q.to_string().contains("Heimdall") && !q.to_string().contains("User"), "{q}");
        assert!(q["criteria"].get("not_for_heimdall").is_some(), "option keys stay");
        assert_eq!(Names::new("Hi Ivy", "Evelyn").wake, [vec!["ivy".to_string()]], "a leading greeting is not the name");
        let multi = Names::new("Mister Bean", "Evelyn");
        let w = words("ok mister bean open it");
        assert!(multi.mentioned(&w) && !multi.mentioned(&words("mister open it")));
    }
}
