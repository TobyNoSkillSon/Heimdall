//! Who the assistant is ("Heimdall is just the default for me … they want a female voice and the
//! name Ivy"): its name, the user's name and the mark in the menu bar and the notch. The wake word is the name itself
//! ("Heimdall, hello" or "Ivy, hello"); there is no separate wake-words setting. Saved in
//! settings.json as `identity` (crates/app `settings.rs`); the app sends it as `UiUpdate::Identity`, the menu's
//! Personalise rows answer with `UiAction::Identity`, and the agent's `personalise` tool changes the same setting.
//!
//! The app, its bundle and binaries keep the name Heimdall. Everything that says the name to the user reads `name()`.
//! Internal labels (the transcript's `User` and `Heimdall` speakers, delivery markers) stay as they are.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

pub const DEFAULT_NAME: &str = "Heimdall";
/// The user's name the personal build was written and its decider trained for, and the internal label of the user's own
/// lines in every edition (text written with it is renamed by `named`).
pub const DEFAULT_USER: &str = "User";
/// The public edition's user until they choose a name (Personalisation ▸ Name & Icon, or the agent's `personalise`):
/// neutral, never the owner's name (VM finding P-11, 28 Oct). Only a label: no claim that a decider trained on
/// "the user" generalises to other names, which stays a separate release gate.
pub const PUBLIC_USER: &str = "User";
/// The heading of the user's own typed messages in the Agent windows: the personal build's stored label, the public one's You
/// (VM run 5 Nov 05:43: never the owner's name in a stranger's app).
pub const USER_HEADING: &str = if cfg!(feature = "public") { "You" } else { DEFAULT_USER };
/// The user in a sentence the app writes about them (a reload's reason): the stored label, or the public edition's "The user".
pub const USER_SUBJECT: &str = if cfg!(feature = "public") { "The user" } else { DEFAULT_USER };
/// A speaker's name as the windows show it: the user's own label (`DEFAULT_USER` in every edition's store) is the public
/// edition's chosen name, or You (VM run 5 Nov 05:43); everyone else as named.
pub fn shown_speaker(speaker: &str) -> String {
    if !cfg!(feature = "public") || speaker != DEFAULT_USER { return speaker.to_string(); }
    let user = current().user_name;
    if user == PUBLIC_USER || user == DEFAULT_USER || user.is_empty() { USER_HEADING.into() } else { user }
}

/// The wake instruction's heading, as the app stores it and as the public edition sends it (agent `identity`).
pub const WAKE_MARKERS: [&str; 2] = ["\nOwner's wake instruction:\n", "\nThe user's wake instruction:\n"];

/// A wake split at its instruction, whichever edition worded it (the last heading).
pub fn split_wake(text: &str) -> Option<(&str, &str)> {
    let (at, len) = WAKE_MARKERS.iter().filter_map(|m| text.rfind(m).map(|i| (i, m.len()))).max_by_key(|(i, _)| *i)?;
    Some((&text[..at], &text[at + len..]))
}
/// Names: letters (any script), digits, spaces, hyphens and apostrophes, at most this many characters.
pub const MAX_NAME: usize = 32;
/// Greetings that lead a wake ("Hey Ivy", "Ivy, hello"); a name made only of them would wake on every "okay".
pub const GREETINGS: [&str; 7] = ["hey", "hi", "hello", "ok", "okay", "yo", "wake up"];

/// The mark in the menu bar and the notch. Every choice draws as a one-colour template in the bar's own ink.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Icon {
    /// Heimdall's helmet (`mac/helmet.rs`), the default.
    #[default]
    Helmet,
    /// An SF Symbol by name: one of `BUNDLED` or any other the system has.
    Symbol { name: String },
    /// An image file: PNG, PDF or SVG (one the agent drew, kept in `<data>/identity/`). Drawn as a template.
    Image { path: PathBuf },
}

/// The bundled marks the Personalise grid offers, after the helmet: SF Symbols that read at 16 pt.
pub const BUNDLED: [&str; 11] = ["sparkle", "leaf", "moon.stars", "bird", "cat", "hare", "flame", "bolt", "crown", "eye", "atom"];

/// The prototype's personas (`MARK_NAMES`, `MARK_VOICES` in lab/setup-wizard; NOTES 28 Oct 19:30 and 19:46): each mark's
/// default name, which is also its wake word, and its Phonon S voice. Heimdall keeps Edwin.
pub const PERSONAS: [(&str, &str, &str); 12] = [
    ("helmet", "Heimdall", "Edwin"),
    ("sparkle", "Lumi", "Lottie"),
    ("leaf", "Ivy", "Everly"),
    ("moon.stars", "Selene", "Bryony"),
    ("bird", "Wren", "Juniper"),
    ("cat", "Felix", "Hugh"),
    ("hare", "Hazel", "Pippa"),
    ("flame", "Ember", "Delia"),
    ("bolt", "Volta", "Russell"),
    ("crown", "Rex", "Miles"),
    ("eye", "Argus", "Preston"),
    ("atom", "Niko", "Micah"),
];

/// A mark's persona: (name, voice). An image the agent drew has none.
pub fn persona(icon: &Icon) -> Option<(&'static str, &'static str)> {
    let key = match icon {
        Icon::Helmet => "helmet",
        Icon::Symbol { name } => name.as_str(),
        Icon::Image { .. } => return None,
    };
    PERSONAS.iter().find(|(k, _, _)| *k == key).map(|(_, n, v)| (*n, *v))
}

/// A settings file from before 26 Oct 16:43 may carry `wake_words`: it still loads, and the field is ignored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    /// What the assistant is called: the menu, the islands, the agent's prompt, the wake spotter and the decider.
    pub name: String,
    /// What the assistant calls the user.
    pub user_name: String,
    #[serde(skip_serializing_if = "is_helmet")]
    pub icon: Icon,
}

fn is_helmet(icon: &Icon) -> bool {
    *icon == Icon::Helmet
}

impl Default for Identity {
    fn default() -> Self {
        let user = if cfg!(feature = "public") { PUBLIC_USER } else { DEFAULT_USER };
        Self { name: DEFAULT_NAME.into(), user_name: user.into(), icon: Icon::Helmet }
    }
}

impl Identity {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    /// The names the user's own build was trained and written for: no header, no prompt fill, no other spellings.
    pub fn default_names(&self) -> bool {
        self.name == DEFAULT_NAME && self.user_name == DEFAULT_USER
    }
    /// The name's words the wake spotter listens for, lower case ("Mary-Jane" → ["mary", "jane"]).
    pub fn wake_name(&self) -> Vec<String> {
        name_tokens(&self.name)
    }
    /// Every check a saved or requested identity must pass; the message says what is wrong in plain words.
    pub fn validate(&self) -> Result<(), String> {
        valid_name(&self.name, "The name")?;
        valid_name(&self.user_name, "Your name")?;
        if name_tokens(&self.name).is_empty() {
            return Err("A greeting can't be the name".into());
        }
        if let Icon::Symbol { name } = &self.icon {
            if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.') {
                return Err("An SF Symbol name is letters, digits and dots".into());
            }
        }
        if let Icon::Image { path } = &self.icon {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if !path.is_absolute() || !matches!(ext.as_str(), "png" | "pdf" | "svg") {
                return Err("An icon image is an absolute path to a PNG, PDF or SVG".into());
            }
        }
        Ok(())
    }
    /// Text written for Heimdall and the user, with this identity's names: whole words only, never inside a path, a file
    /// name or an identifier (`Heimdall.app`, `Projects/Heimdall`, `mcp__heimdall__`), never inside backticks.
    pub fn named(&self, text: &str) -> String {
        if self.default_names() {
            return text.to_string();
        }
        replace_names(text, &[(DEFAULT_NAME, &self.name), (DEFAULT_USER, &self.user_name)])
    }
}

/// The lower-case words of a name after any greeting ("Hey Ivy" → ["ivy"], "Wake up Ivy" → ["ivy"]).
pub fn name_tokens(wake: &str) -> Vec<String> {
    let lower = wake.to_lowercase();
    let mut rest = lower.trim();
    loop {
        let before = rest;
        for g in GREETINGS {
            if let Some(after) = rest.strip_prefix(g) {
                if after.is_empty() || after.starts_with([' ', ',']) {
                    rest = after.trim_start_matches([' ', ',']);
                }
            }
        }
        if rest == before {
            break;
        }
    }
    rest.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

fn valid_name(s: &str, what: &str) -> Result<(), String> {
    let t = s.trim();
    if t.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if t != s || t.chars().count() > MAX_NAME {
        return Err(format!("{what} is at most {MAX_NAME} characters, without leading or trailing spaces"));
    }
    if !t.chars().any(char::is_alphabetic) || !t.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '\'' | '’')) {
        return Err("Only letters, numbers, - and '".into());
    }
    Ok(())
}

/// Whole-word replacement that leaves paths, file names, identifiers and backticked spans alone.
fn replace_names(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut code = false;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if c == '`' {
            code = !code;
            out.push(c);
            i += 1;
            continue;
        }
        if !code {
            if let Some((from, to)) = pairs.iter().find(|(from, _)| text[at..].starts_with(from)) {
                let end = at + from.len();
                let prev = text[..at].chars().next_back();
                let next = text[end..].chars().next();
                let after_next = text[end..].chars().nth(1);
                let word_start = prev.is_none_or(|p| !(p.is_alphanumeric() || matches!(p, '_' | '/' | '.' | '-' | '[')));
                let word_end = next.is_none_or(|n| !(n.is_alphanumeric() || matches!(n, '_' | '/' | '-')))
                    && !(next == Some('.') && after_next.is_some_and(char::is_alphanumeric));
                if word_start && word_end {
                    out.push_str(to);
                    i += from.chars().count();
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

static CURRENT: RwLock<Option<Identity>> = RwLock::new(None);

/// The identity the UI shows; the default until the app sends one.
pub fn current() -> Identity {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}
pub fn set(identity: Identity) {
    *CURRENT.write().unwrap() = Some(identity);
}
/// The assistant's name as the user sees it.
pub fn name() -> String {
    current().name
}
/// The name app chrome uses (Setup's title, the Quit row): the public edition's product name, "Heimdall Alpha", while
/// the assistant keeps its default name; a name the user chose wins in both editions (VM finding P-07, 28 Oct).
pub fn app_title() -> String {
    title_for(&current().name)
}
pub fn title_for(name: &str) -> String {
    if cfg!(feature = "public") && name == DEFAULT_NAME { "Heimdall Alpha".into() } else { name.into() }
}
/// `Identity::named` with the current identity: user-facing text written with Heimdall's and the user's names.
pub fn named(text: &str) -> String {
    current().named(text)
}

/// The menu's Personalise rows → crates/app (`UiAction::Identity`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityAction {
    SetName(String),
    SetUserName(String),
    SetIcon(Icon),
}

impl Identity {
    /// The menu's change applied; Err with the reason when the result would not validate (nothing changes then).
    pub fn apply(&mut self, action: &IdentityAction) -> Result<bool, String> {
        let mut next = self.clone();
        match action {
            IdentityAction::SetName(n) => next.name = n.trim().to_string(),
            IdentityAction::SetUserName(n) => next.user_name = n.trim().to_string(),
            IdentityAction::SetIcon(icon) => next.icon = icon.clone(),
        }
        next.validate()?;
        let changed = next != *self;
        *self = next;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ivy() -> Identity {
        Identity { name: "Ivy".into(), user_name: "Evelyn".into(), ..Default::default() }
    }

    #[cfg(not(feature = "public"))] // the personal edition's default
    #[test]
    fn the_default_is_heimdall_and_owner_and_writes_nothing_extra() {
        let d = Identity::default();
        assert!(d.is_default() && d.default_names());
        assert_eq!(serde_json::to_value(&d).unwrap(), serde_json::json!({"name":"Heimdall","user_name":"User"}));
        assert_eq!(serde_json::from_str::<Identity>("{}").unwrap(), d);
        assert_eq!(d.wake_name(), ["heimdall"]);
        assert_eq!(d.named("Quit Heimdall"), "Quit Heimdall");
    }

    /// P-11: the public user is "User" until they choose a name; text written with the user's name says User; a chosen
    /// name (and a saved file naming the user) loads unchanged.
    #[cfg(feature = "public")]
    #[test]
    fn the_public_default_user_is_neutral_and_chosen_names_survive() {
        let d = Identity::default();
        assert_eq!((d.name.as_str(), d.user_name.as_str()), ("Heimdall", "User"));
        assert!(d.is_default() && d.default_names() && d.validate().is_ok());
        assert_eq!(d.named("User's Mac. User: hi."), "User's Mac. User: hi.");
        assert_eq!(serde_json::from_str::<Identity>("{}").unwrap().user_name, "User");
        let saved: Identity = serde_json::from_str(r#"{"name":"Ivy","user_name":"Evelyn"}"#).unwrap();
        assert_eq!((saved.name.as_str(), saved.user_name.as_str()), ("Ivy", "Evelyn"));
        let owner: Identity = serde_json::from_str(r#"{"name":"Heimdall","user_name":"User"}"#).unwrap();
        assert_eq!(owner.user_name, "User");
    }

    #[test]
    fn app_chrome_says_the_product_name_only_for_the_default_assistant_name() {
        let product = if cfg!(feature = "public") { "Heimdall Alpha" } else { "Heimdall" };
        assert_eq!(title_for(DEFAULT_NAME), product);
        assert_eq!(title_for("Ivy"), "Ivy");
    }

    #[test]
    fn named_swaps_whole_words_and_leaves_paths_and_code_alone() {
        let i = ivy();
        assert_eq!(i.named("Quit Heimdall"), "Quit Ivy");
        assert_eq!(i.named("Heimdall's tools, User's Mac. User: hi."), "Ivy's tools, Evelyn's Mac. Evelyn: hi.");
        assert_eq!(i.named("Projects/Heimdall and Heimdall.app and mcp__heimdall__x"), "Projects/Heimdall and Heimdall.app and mcp__heimdall__x");
        assert_eq!(i.named("`[Mic · User] words` said User"), "`[Mic · User] words` said Evelyn");
        assert_eq!(i.named("[Heimdall delivery: d1] TobyNoSkillSon Heimdall."), "[Heimdall delivery: d1] TobyNoSkillSon Ivy.");
    }

    #[test]
    fn the_wake_is_the_name() {
        let mut i = Identity::default();
        assert!(i.apply(&IdentityAction::SetName("Ivy".into())).unwrap());
        assert_eq!(i.wake_name(), ["ivy"]);
        i.apply(&IdentityAction::SetName("Mary-Jane".into())).unwrap();
        assert_eq!(i.wake_name(), ["mary", "jane"]);
        assert_eq!(name_tokens("Wake up, Ivy"), ["ivy"]);
        assert!(i.apply(&IdentityAction::SetName("".into())).is_err());
        assert!(i.apply(&IdentityAction::SetName("Okay".into())).is_err(), "a greeting would wake on every okay");
        assert_eq!(i.name, "Mary-Jane", "a refused change leaves it as it was");
    }

    /// No Wake Words field. A settings file written before still loads; the field is dropped.
    #[test]
    fn an_old_identity_with_wake_words_still_loads() {
        let old = serde_json::json!({"name":"Ivy","user_name":"Evelyn","wake_words":["Hey Ivy","Ivy","Ivey"],"icon":{"kind":"symbol","name":"leaf"}});
        let i: Identity = serde_json::from_value(old).unwrap();
        assert_eq!(i, Identity { icon: Icon::Symbol { name: "leaf".into() }, ..ivy() });
        assert!(serde_json::to_value(&i).unwrap().get("wake_words").is_none());
    }

    #[test]
    fn icons_round_trip_and_bad_ones_are_refused() {
        let mut i = ivy();
        i.icon = Icon::Symbol { name: "leaf".into() };
        let v = serde_json::to_value(&i).unwrap();
        assert_eq!(v["icon"], serde_json::json!({"kind":"symbol","name":"leaf"}));
        assert_eq!(serde_json::from_value::<Identity>(v).unwrap(), i);
        i.icon = Icon::Image { path: "relative.svg".into() };
        assert!(i.validate().is_err());
        i.icon = Icon::Image { path: "/tmp/mark.svg".into() };
        assert!(i.validate().is_ok());
    }

    /// VM run 5 Nov 05:43: the public windows never show the owner's name for the user's own lines.
    #[test]
    fn the_users_own_label_shows_as_the_editions_user() {
        assert_eq!(shown_speaker("Dave"), "Dave");
        if cfg!(feature = "public") {
            assert_ne!(shown_speaker(DEFAULT_USER), DEFAULT_USER);
            assert_eq!((USER_HEADING, USER_SUBJECT), ("You", "The user"));
        } else {
            assert_eq!(shown_speaker(DEFAULT_USER), DEFAULT_USER);
        }
        for m in WAKE_MARKERS { assert_eq!(split_wake(&format!("data{m}go")), Some(("data", "go"))); }
    }
}
