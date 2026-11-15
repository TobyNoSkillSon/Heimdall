//! The agent's `computer` tool (bug note 2026-10-22T19-16-01): act on another app the way the user would, through the
//! Accessibility API., after Heimdall could not mute a Discord user for him: "Doesn't have computer
//! control." Built so it cannot surprise him:
//!
//! - Off until he turns it on (`gate::set_enabled`, the menu's Control Apps row; settings key `computer_control`).
//! - Accessibility first: elements are found by role and title in one named app and pressed, opened (the context menu
//!   a right-click shows), picked, set or typed into through AXUIElement. Nothing moves the pointer or sends keys.
//! - A pointer click (CGEvent, posted to the app's process so the pointer stays where it is) is the fallback, off by
//!   default (`gate::set_clicks`, the user's decision), and never while the user has touched the mouse or keyboard within
//!   [`POINTER_IDLE_S`].
//! - Refused: Heimdall itself and its islands (the deny list refuses presses on Heimdall), system settings, password
//!   managers and the Keychain, terminals and script runners (typing there would run commands the deny list judges),
//!   any window with a password field, a payment form, or a title about passwords or payment, and buttons that buy or
//!   pay.
//! - Every answer is read back from the tree after the action (a menu item's tick, a checkbox, a field's text), and a
//!   miss names the closest matches.
//!
//! The logic here runs over [`Backend`]; `mac::Mac` is the real one, `fake::Fake` the tests' tree.
use anyhow::{bail, Result};
use serde_json::Value;
use std::fmt::Write as _;

pub mod asks;
pub mod fake;
#[cfg(target_os = "macos")]
pub mod mac;

/// A pointer click waits until the user has not touched the mouse or keyboard for this long.
pub const POINTER_IDLE_S: f64 = 2.0;
/// At most this many lines in a `find` answer.
pub const FIND_LINES: usize = 40;
/// At most this many candidates listed when a title is ambiguous or missing.
pub const LIST_MAX: usize = 8;
/// `find` while Seeing is off (review C4, night 10-11 Nov).
pub const SEEING_OFF: &str = "SEEING_OFF: Seeing is off ({user}'s privacy switch), so no app's window text is read: find is refused, and an action reads back only the element it acted on. Act by the name {user} gave (press, menu, pick), or tell them Seeing is off; turn it on with settings only if they ask.";

/// The switches, read on every call. Both off by default.
pub mod gate {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ENABLED: AtomicBool = AtomicBool::new(false);
    static CLICKS: AtomicBool = AtomicBool::new(false);
    /// Control Apps (menu) / `computer_control` (settings tool).
    pub fn set_enabled(on: bool) {
        ENABLED.store(on, Ordering::Release)
    }
    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Acquire)
    }
    /// Pointer clicks as the fallback (`settings.json` `computer_clicks`; off until the user decides).
    pub fn set_clicks(on: bool) {
        CLICKS.store(on, Ordering::Release)
    }
    pub fn clicks() -> bool {
        CLICKS.load(Ordering::Acquire)
    }
    #[derive(Clone, Copy, Debug)]
    pub struct Gate {
        pub enabled: bool,
        pub clicks: bool,
        /// Seeing (the user's privacy switch, the app's `screen_off`) is on. Off, no window's text is read to the agent:
        /// `find` is refused and an answer reads back only the element acted on (review C4, night 10-11 Nov).
        pub seeing: bool,
    }
    /// The switches now, with Seeing as the app holds it.
    pub fn now(seeing: bool) -> Gate {
        Gate { enabled: enabled(), clicks: clicks(), seeing }
    }
}
pub use gate::Gate;

/// One element as read from the tree. `handle` names it to the backend for actions and re-reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub handle: usize,
    pub role: String,
    pub subrole: String,
    pub title: String,
    pub description: String,
    pub value: String,
    pub help: String,
    pub identifier: String,
    pub placeholder: String,
    pub enabled: bool,
    pub focused: bool,
    /// `AXMenuItemMarkChar`: a menu item's tick ("✓") when set.
    pub mark: String,
    /// Screen points from the top-left of the main display: x, y, width, height.
    pub frame: Option<(f64, f64, f64, f64)>,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
    pub pid: i32,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub root: Node,
    /// The walk stopped at its node, depth or time budget.
    pub truncated: bool,
}

/// The Accessibility API as the tool uses it.
pub trait Backend {
    /// Heimdall holds the Accessibility permission. Never prompts.
    fn trusted(&self) -> bool;
    /// The running apps with a Dock presence (and accessory apps), with their process ids.
    fn apps(&self) -> Vec<AppInfo>;
    /// The app's tree (windows, open menus), without the menu bar.
    fn snapshot(&self, pid: i32) -> Result<Snapshot>;
    /// The element's action names (`AXPress`, `AXShowMenu`, `AXPick`, `AXConfirm`, ...).
    fn actions(&self, handle: usize) -> Vec<String>;
    fn perform(&self, handle: usize, action: &str) -> Result<()>;
    /// Sets a string attribute (`AXValue`, `AXSelectedText`).
    fn set_text(&self, handle: usize, attribute: &str, text: &str) -> Result<()>;
    fn focus(&self, handle: usize) -> Result<()>;
    /// The element's attributes now (children left empty); None when it is gone.
    fn reread(&self, handle: usize) -> Option<Node>;
    /// The app's focused element.
    fn focused(&self, pid: i32) -> Option<Node>;
    /// Seconds since the user last touched the mouse, trackpad or keyboard.
    fn idle_seconds(&self) -> f64;
    /// A click at a screen point posted to `pid` only: the pointer does not move.
    fn click(&self, pid: i32, x: f64, y: f64, right: bool) -> Result<()>;
    /// Lets the app redraw after an action before it is read back.
    fn settle(&self, millis: u64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Find,
    Press,
    Menu,
    Pick,
    Set,
    Type,
    Click,
}
impl Action {
    pub const NAMES: [&'static str; 7] = ["find", "press", "menu", "pick", "set", "type", "click"];
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "find" => Self::Find,
            "press" => Self::Press,
            "menu" => Self::Menu,
            "pick" => Self::Pick,
            "set" => Self::Set,
            "type" => Self::Type,
            "click" => Self::Click,
            _ => return None,
        })
    }
}

/// One call, as the MCP schema delivers it (validated there; checked again here for the rules it cannot say).
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub action: Action,
    pub app: String,
    pub role: Option<String>,
    pub title: Option<String>,
    /// 1-based, among equally good matches.
    pub index: Option<usize>,
    /// `set`'s value, `type`'s text.
    pub text: Option<String>,
    pub point: Option<(f64, f64)>,
    pub right: bool,
    /// The user's own recent words naming an outward button or item (Send, Delete, Allow), quoted (review C3).
    pub owner_asked: Option<String>,
}

impl Request {
    pub fn from_args(args: &Value) -> Result<Self> {
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned);
        let Some(action) = args.get("action").and_then(Value::as_str).and_then(Action::parse) else {
            bail!("INVALID_ARGUMENT: action: one of {}", Action::NAMES.join(", "))
        };
        let Some(app) = s("app") else { bail!("INVALID_ARGUMENT: app: the app's name as the Dock shows it (\"Discord\")") };
        let text = args.get("text").and_then(Value::as_str).map(str::to_owned);
        let point = match (args.get("x").and_then(Value::as_f64), args.get("y").and_then(Value::as_f64)) {
            (Some(x), Some(y)) => Some((x, y)),
            (None, None) => None,
            _ => bail!("INVALID_ARGUMENT: x and y go together"),
        };
        let r = Request {
            action,
            app,
            role: s("role"),
            title: s("title"),
            index: args.get("index").and_then(Value::as_u64).map(|i| i as usize),
            text,
            point,
            right: args.get("button").and_then(Value::as_str) == Some("right"),
            owner_asked: s("owner_asked"),
        };
        match r.action {
            Action::Press | Action::Menu | Action::Pick if r.title.is_none() && r.role.is_none() => {
                bail!("INVALID_ARGUMENT: title: name the element (its title or label); find lists them")
            }
            Action::Set if r.text.is_none() => bail!("INVALID_ARGUMENT: text: set needs the value to write"),
            Action::Set if r.title.is_none() && r.role.is_none() => bail!("INVALID_ARGUMENT: title: name the field to set"),
            Action::Type if r.text.as_deref().is_none_or(str::is_empty) => bail!("INVALID_ARGUMENT: text: type needs the text"),
            Action::Click if r.point.is_none() && r.title.is_none() => {
                bail!("INVALID_ARGUMENT: click needs x and y (screen points, as find gives them) or a title")
            }
            _ => {}
        }
        if r.point.is_some() && r.action != Action::Click {
            bail!("INVALID_ARGUMENT: x and y are for click only");
        }
        if r.text.as_deref().is_some_and(|t| t.contains('\0')) {
            bail!("INVALID_ARGUMENT: text: no NUL characters");
        }
        // A line break can submit a form or run a command; Return is the user's (review 10 Nov).
        if r.text.as_deref().is_some_and(|t| t.contains(['\n', '\r', '\u{2028}', '\u{2029}'])) {
            bail!("INVALID_ARGUMENT: text: one line only; a line break would send or run it, which stays {{user}}'s");
        }
        Ok(r)
    }
}

// ---- refusals ----

/// Apps the tool never acts on, by bundle id prefix: Heimdall and its helpers, the system's settings and security
/// surfaces, password managers, and apps that run typed commands (their commands would skip the deny list).
const REFUSED_BUNDLES: &[(&str, &str)] = &[
    ("dev.heimdall", "Heimdall itself"),
    ("com.tobynoskillson.heimdall", "Heimdall itself"),
    ("com.apple.systempreferences", "system settings"),
    ("com.apple.SystemSettings", "system settings"),
    ("com.apple.Settings", "system settings"),
    ("com.apple.preference.", "system settings"),
    ("com.apple.SecurityAgent", "a password prompt"),
    ("com.apple.LocalAuthentication", "a password prompt"),
    ("com.apple.coreautha", "a password prompt"),
    ("com.apple.loginwindow", "the login window"),
    ("com.apple.keychainaccess", "the Keychain"),
    ("com.apple.Passwords", "passwords"),
    ("com.apple.PassKit", "payment"),
    ("com.apple.Wallet", "payment"),
    ("com.agilebits.", "passwords"),
    ("com.1password.", "passwords"),
    ("com.bitwarden.", "passwords"),
    ("com.lastpass.", "passwords"),
    ("com.dashlane.", "passwords"),
    ("com.apple.Terminal", "a terminal (typed commands skip the deny list)"),
    ("com.googlecode.iterm2", "a terminal (typed commands skip the deny list)"),
    ("com.mitchellh.ghostty", "a terminal (typed commands skip the deny list)"),
    ("dev.warp.", "a terminal (typed commands skip the deny list)"),
    ("net.kovidgoyal.kitty", "a terminal (typed commands skip the deny list)"),
    ("org.alacritty", "a terminal (typed commands skip the deny list)"),
    ("io.alacritty", "a terminal (typed commands skip the deny list)"),
    ("com.github.wez.wezterm", "a terminal (typed commands skip the deny list)"),
    ("com.apple.ScriptEditor", "a script editor (scripts skip the deny list)"),
    ("com.apple.Automator", "a script editor (scripts skip the deny list)"),
    ("com.apple.shortcuts", "a script editor (scripts skip the deny list)"),
    // Review 10 Nov: editors with a built-in terminal, more terminals, agent apps (a prompt sent there runs an agent),
    // Activity Monitor (it can quit Heimdall) and Finder (it can delete past the deny list).
    ("com.microsoft.VSCode", "an editor with a terminal (typed commands skip the deny list)"),
    ("com.todesktop.", "an editor with a terminal (typed commands skip the deny list)"),
    ("dev.zed.", "an editor with a terminal (typed commands skip the deny list)"),
    ("com.jetbrains.", "an editor with a terminal (typed commands skip the deny list)"),
    ("com.apple.dt.Xcode", "an editor that runs code (it skips the deny list)"),
    ("co.zeit.hyper", "a terminal (typed commands skip the deny list)"),
    ("org.tabby", "a terminal (typed commands skip the deny list)"),
    ("com.termius", "a terminal (typed commands skip the deny list)"),
    ("com.raphaelamorim.rio", "a terminal (typed commands skip the deny list)"),
    ("com.t3tools.", "an agent app (a prompt sent there runs an agent)"),
    ("com.anthropic.", "an agent app (a prompt sent there runs an agent)"),
    ("com.openai.", "an agent app (a prompt sent there runs an agent)"),
    ("com.apple.ActivityMonitor", "Activity Monitor (it can quit Heimdall)"),
    ("com.apple.finder", "Finder (it deletes past the deny list)"),
    // Apps whose ordinary buttons erase, install, run scripts or approve, the user's X browser, and Arsenal (its approval
    // window takes his yes only).
    ("com.apple.DiskUtility", "Disk Utility (it erases disks)"),
    ("com.apple.installer", "the installer (installing is {user}'s)"),
    ("com.apple.AppStore", "the App Store (installing and buying are {user}'s)"),
    ("com.raycast.", "a launcher that runs commands (they skip the deny list)"),
    ("com.runningwithcrayons.Alfred", "a launcher that runs commands (they skip the deny list)"),
    ("com.stairways.keyboardmaestro", "an automation app (its macros skip the deny list)"),
    ("org.hammerspoon.", "an automation app (its scripts skip the deny list)"),
    ("com.exafunction.windsurf", "an editor with a terminal (typed commands skip the deny list)"),
    ("dev.arsenal.", "Arsenal (its approvals are {user}'s own yes)"),
    ("com.citrolabs.ego", "Ego, an agent browser (drive it with its own tools, never through Accessibility)"),
    ("me.proton.pass", "passwords"),
];
/// The same by name, for apps whose bundle id is unknown or a lab build (the islands helper is `FloatingIslands`).
const REFUSED_NAMES: &[(&str, &str)] = &[
    ("heimdall", "Heimdall itself"),
    ("heimdall alpha", "Heimdall itself"),
    ("floatingislands", "Heimdall itself"),
    ("system settings", "system settings"),
    ("system preferences", "system settings"),
    ("keychain access", "the Keychain"),
    ("passwords", "passwords"),
    ("terminal", "a terminal (typed commands skip the deny list)"),
    ("t3 code", "an agent app (a prompt sent there runs an agent)"),
    ("claude", "an agent app (a prompt sent there runs an agent)"),
    ("codex", "an agent app (a prompt sent there runs an agent)"),
    ("chatgpt", "an agent app (a prompt sent there runs an agent)"),
    ("cursor", "an editor with a terminal (typed commands skip the deny list)"),
    ("code", "an editor with a terminal (typed commands skip the deny list)"),
    ("visual studio code", "an editor with a terminal (typed commands skip the deny list)"),
    ("zed", "an editor with a terminal (typed commands skip the deny list)"),
    ("activity monitor", "Activity Monitor (it can quit Heimdall)"),
    ("finder", "Finder (it deletes past the deny list)"),
    ("disk utility", "Disk Utility (it erases disks)"),
    ("installer", "the installer (installing is {user}'s)"),
    ("app store", "the App Store (installing and buying are {user}'s)"),
    ("raycast", "a launcher that runs commands (they skip the deny list)"),
    ("alfred", "a launcher that runs commands (they skip the deny list)"),
    ("keyboard maestro", "an automation app (its macros skip the deny list)"),
    ("hammerspoon", "an automation app (its scripts skip the deny list)"),
    ("windsurf", "an editor with a terminal (typed commands skip the deny list)"),
    ("arsenal", "Arsenal (its approvals are {user}'s own yes)"),
    ("ego lite", "Ego, an agent browser (drive it with its own tools, never through Accessibility)"),
    ("ego", "Ego, an agent browser (drive it with its own tools, never through Accessibility)"),
    ("proton pass", "passwords"),
];

pub fn refused_app(app: &AppInfo) -> Option<&'static str> {
    let id = app.bundle_id.to_lowercase();
    let name = app.name.trim().to_lowercase();
    REFUSED_BUNDLES
        .iter()
        .find(|(prefix, _)| id.starts_with(&prefix.to_lowercase()))
        .or_else(|| REFUSED_NAMES.iter().find(|(n, _)| name == *n || name.starts_with(&format!("{n} "))))
        .map(|(_, why)| *why)
        .or_else(|| (app.pid as u32 == std::process::id()).then_some("Heimdall itself"))
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}
fn has_phrase(text: &str, phrase: &[&str]) -> bool {
    let w = words(text);
    w.windows(phrase.len()).any(|win| win.iter().zip(phrase).all(|(a, b)| a == b))
}
/// A window title about passwords, payment or sign-in secrets.
fn sensitive_title(title: &str) -> bool {
    const PHRASES: &[&[&str]] = &[
        &["password"], &["passwords"], &["passcode"], &["passkey"], &["passkeys"], &["keychain"], &["payment"],
        &["payments"], &["checkout"], &["check", "out"], &["billing"], &["purchase"], &["credit", "card"],
        &["apple", "pay"], &["pay", "with"], &["two", "factor"], &["verification", "code"], &["recovery", "key"],
        &["place", "your", "order"], &["your", "order"], &["order", "summary"], &["review", "order"], &["cart"],
        &["basket"],
    ];
    PHRASES.iter().any(|p| has_phrase(title, p))
}
/// A label that buys, pays or subscribes: pressing it spends the user's money.
fn spends(label: &str) -> bool {
    const PHRASES: &[&[&str]] = &[
        &["buy"], &["purchase"], &["pay"], &["payment"], &["checkout"], &["check", "out"], &["place", "order"],
        &["order", "now"], &["subscribe"], &["donate"], &["send", "money"], &["upgrade", "now"], &["confirm", "and", "pay"],
        // Review 10 Nov: Amazon's and stores' wordings, rentals and price buttons.
        &["place", "your", "order"], &["complete", "order"], &["confirm", "order"], &["submit", "order"],
        &["complete", "purchase"], &["rent"], &["tip"], &["transfer"],
    ];
    PHRASES.iter().any(|p| has_phrase(label, p)) || price(label)
}
/// A label that sends or posts, deletes, approves or installs: what it does leaves the window or cannot be undone. The
/// agent may press it only when the user's own recent words name that label (`owner_asked`, checked by
/// [`asks::names_label`]); a message read out of the app (`find`) can never ask for it.
fn outward(label: &str) -> Option<&'static str> {
    const PHRASES: &[(&[&str], &str)] = &[
        (&["send"], "send"), (&["resend"], "send"), (&["forward"], "send"), (&["reply"], "send"), (&["submit"], "send"),
        (&["upload"], "send"), (&["invite"], "send"), (&["email"], "send"), (&["report"], "send"),
        (&["post"], "post"), (&["repost"], "post"), (&["tweet"], "post"), (&["retweet"], "post"), (&["share"], "post"),
        (&["publish"], "post"),
        (&["delete"], "delete"), (&["remove"], "delete"), (&["erase"], "delete"), (&["trash"], "delete"),
        (&["discard"], "delete"), (&["uninstall"], "delete"), (&["wipe"], "delete"), (&["kick"], "delete"),
        (&["ban"], "delete"), (&["unsubscribe"], "delete"),
        (&["allow"], "approve"), (&["approve"], "approve"), (&["grant"], "approve"), (&["trust"], "approve"),
        (&["authorize"], "approve"), (&["authorise"], "approve"), (&["accept"], "approve"), (&["agree"], "approve"),
        (&["confirm"], "approve"), (&["sign"], "approve"), (&["unlock"], "approve"),
        (&["install"], "install"), (&["order"], "buy"),
    ];
    PHRASES.iter().find(|(p, _)| has_phrase(label, p)).map(|(_, what)| *what)
}
/// A price on a button ("$4.99", "4,99 zł", "€10", "12 PLN").
fn price(label: &str) -> bool {
    let l = label.to_lowercase();
    let digit_after = |sym: &str| l.match_indices(sym).any(|(i, s)| l[i + s.len()..].trim_start().starts_with(|c: char| c.is_ascii_digit()));
    let digit_before = |sym: &str| l.match_indices(sym).any(|(i, _)| l[..i].trim_end().ends_with(|c: char| c.is_ascii_digit()));
    ["$", "€", "£", "¥"].iter().any(|s| digit_after(s) || digit_before(s))
        || ["zł", "pln", "usd", "eur", "gbp"].iter().any(|s| digit_before(s))
}
/// A field of a payment form.
fn payment_field(label: &str) -> bool {
    const PHRASES: &[&[&str]] = &[
        &["card", "number"], &["cvv"], &["cvc"], &["security", "code"], &["expiration", "date"], &["expiry"],
        &["iban"], &["cardholder"], &["name", "on", "card"],
    ];
    PHRASES.iter().any(|p| has_phrase(label, p))
}
fn secure(n: &Node) -> bool {
    n.role == "AXSecureTextField" || n.subrole == "AXSecureTextField"
}
fn text_role(n: &Node) -> bool {
    matches!(n.role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox" | "AXSearchField")
}

// ---- the flattened tree ----

struct Flat<'a> {
    node: &'a Node,
    parent: Option<usize>,
    depth: usize,
}
fn flatten(root: &Node) -> Vec<Flat<'_>> {
    fn go<'a>(n: &'a Node, parent: Option<usize>, depth: usize, out: &mut Vec<Flat<'a>>) {
        let me = out.len();
        out.push(Flat { node: n, parent, depth });
        for c in &n.children {
            go(c, Some(me), depth + 1, out);
        }
    }
    let mut out = vec![];
    go(root, None, 0, &mut out);
    out
}
fn ancestors<'a>(flat: &'a [Flat<'a>], i: usize) -> impl Iterator<Item = usize> + 'a {
    std::iter::successors(flat[i].parent, move |p| flat[*p].parent)
}
/// The window (or open menu, for an item in it) an element sits in.
fn container(flat: &[Flat], i: usize) -> Option<usize> {
    std::iter::once(i).chain(ancestors(flat, i)).find(|a| matches!(flat[*a].node.role.as_str(), "AXWindow" | "AXSheet" | "AXMenu"))
}
fn window(flat: &[Flat], i: usize) -> Option<usize> {
    std::iter::once(i).chain(ancestors(flat, i)).find(|a| matches!(flat[*a].node.role.as_str(), "AXWindow" | "AXSheet"))
}
fn in_menu(flat: &[Flat], i: usize) -> bool {
    std::iter::once(i).chain(ancestors(flat, i)).any(|a| flat[a].node.role == "AXMenu")
}
fn subtree(flat: &[Flat], top: usize) -> std::ops::Range<usize> {
    let end = (top + 1..flat.len()).find(|j| flat[*j].depth <= flat[top].depth).unwrap_or(flat.len());
    top..end
}

/// Why the window holding element `i` is off limits, if it is.
fn refused_window(flat: &[Flat], i: usize) -> Option<String> {
    let w = window(flat, i)?;
    let title = &flat[w].node.title;
    if sensitive_title(title) {
        return Some(format!("its window “{title}” is about passwords or payment"));
    }
    for j in subtree(flat, w) {
        let n = flat[j].node;
        if secure(n) {
            return Some("its window has a password field".into());
        }
        if text_role(n) && labels(n).iter().any(|l| payment_field(l)) {
            return Some("its window is a payment form".into());
        }
    }
    None
}

// ---- matching ----

/// The texts an element can be named by, most specific first.
fn labels(n: &Node) -> Vec<&str> {
    let value = if n.value.chars().count() <= 200 { n.value.as_str() } else { "" };
    [n.title.as_str(), n.description.as_str(), value, n.placeholder.as_str(), n.help.as_str(), n.identifier.as_str()]
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}
/// The one name shown for an element; an unnamed one (a web row) goes by its first named descendant's. A text field is
/// named by its title, description or placeholder before its text (the real test named a field by what it held).
fn label(n: &Node) -> String {
    fn inner(n: &Node, depth: usize) -> Option<String> {
        let named = if text_role(n) {
            [n.title.as_str(), n.description.as_str(), n.placeholder.as_str()].into_iter().map(str::trim).find(|s| !s.is_empty())
        } else {
            None
        };
        if let Some(s) = named.or_else(|| labels(n).first().copied()) {
            return Some(s.chars().take(80).collect());
        }
        if depth == 0 {
            return None;
        }
        n.children.iter().find_map(|c| inner(c, depth - 1))
    }
    inner(n, 3).unwrap_or_default()
}
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}
/// 3: the whole label; 2: the label starts with it as words ("morgan, online"); 1: it appears as words; 0: no.
fn score(n: &Node, query: &str) -> u8 {
    let q = norm(query);
    let qw = words(&q);
    labels(n)
        .iter()
        .map(|l| {
            let l = norm(l);
            if l == q || words(&l) == qw && !qw.is_empty() {
                3
            } else if !qw.is_empty() && words(&l).starts_with(&qw) {
                2
            } else if !qw.is_empty() && has_phrase(&l, &qw.iter().map(String::as_str).collect::<Vec<_>>()) {
                1
            } else {
                0
            }
        })
        .max()
        .unwrap_or(0)
}

/// Accepted role words and the AX roles (or subroles) they mean.
const ROLES: &[(&str, &[&str])] = &[
    ("button", &["AXButton"]),
    ("menu item", &["AXMenuItem", "AXMenuBarItem"]),
    ("checkbox", &["AXCheckBox"]),
    ("switch", &["AXSwitch", "AXCheckBox"]),
    ("radio", &["AXRadioButton"]),
    ("tab", &["AXTab", "AXRadioButton", "AXTabButton"]),
    ("field", &["AXTextField", "AXTextArea", "AXComboBox", "AXSearchField"]),
    ("text field", &["AXTextField", "AXSearchField", "AXComboBox"]),
    ("text area", &["AXTextArea"]),
    ("text", &["AXStaticText"]),
    ("link", &["AXLink"]),
    ("row", &["AXRow", "AXOutlineRow", "AXTableRow"]),
    ("cell", &["AXCell"]),
    ("popup", &["AXPopUpButton", "AXMenuButton"]),
    ("slider", &["AXSlider"]),
    ("image", &["AXImage"]),
    ("group", &["AXGroup"]),
    ("list", &["AXList", "AXTable", "AXOutline"]),
    ("menu", &["AXMenu"]),
    ("window", &["AXWindow", "AXSheet"]),
];
fn role_set(role: &str) -> Result<Vec<String>> {
    let r = norm(role).replace(['_', '-'], " ");
    let r = r.trim_end_matches('s');
    if role.trim().starts_with("AX") {
        return Ok(vec![role.trim().to_owned()]);
    }
    let alias = match r {
        "menuitem" => "menu item",
        "textfield" | "input" | "search field" => "text field",
        "textarea" => "text area",
        "static text" | "label" => "text",
        "check box" | "toggle" => "checkbox",
        "radio button" => "radio",
        "pop up" | "popup button" | "dropdown" | "drop down" => "popup",
        other => other,
    };
    match ROLES.iter().find(|(w, _)| *w == alias) {
        Some((_, roles)) => Ok(roles.iter().map(|s| s.to_string()).collect()),
        None => bail!(
            "INVALID_ARGUMENT: role: “{role}” is not one of {} (or an AX role such as AXButton)",
            ROLES.iter().map(|(w, _)| *w).collect::<Vec<_>>().join(", ")
        ),
    }
}
fn role_matches(n: &Node, roles: &Option<Vec<String>>) -> bool {
    roles.as_ref().is_none_or(|rs| rs.iter().any(|r| *r == n.role || *r == n.subrole))
}
/// How an element reads to the agent: `button “Mute”`.
fn kind(n: &Node) -> String {
    let base = match n.subrole.as_str() {
        "AXSwitch" => "switch",
        "AXSecureTextField" => "password field",
        _ => match n.role.as_str() {
            "AXButton" => "button",
            "AXMenuItem" | "AXMenuBarItem" => "menu item",
            "AXCheckBox" => "checkbox",
            "AXRadioButton" => "radio",
            "AXTextField" | "AXSearchField" | "AXComboBox" => "text field",
            "AXTextArea" => "text area",
            "AXStaticText" => "text",
            "AXLink" => "link",
            "AXRow" | "AXOutlineRow" | "AXTableRow" => "row",
            "AXCell" => "cell",
            "AXPopUpButton" | "AXMenuButton" => "popup",
            "AXSlider" => "slider",
            "AXImage" => "image",
            "AXGroup" => "group",
            "AXList" | "AXTable" | "AXOutline" => "list",
            "AXMenu" => "menu",
            "AXWindow" => "window",
            "AXSheet" => "sheet",
            "AXWebArea" => "web page",
            "AXTabGroup" => "tab group",
            "AXToolbar" => "toolbar",
            "AXScrollArea" => "scroll area",
            "AXSplitGroup" => "split group",
            other => return other.trim_start_matches("AX").to_lowercase(),
        },
    };
    base.into()
}
fn describe(n: &Node) -> String {
    let l = label(n);
    if l.is_empty() { kind(n) } else { format!("{} “{l}”", kind(n)) }
}
/// A tick or checkbox state, when the element has one.
fn checked(n: &Node) -> Option<bool> {
    if n.role == "AXMenuItem" {
        if !n.mark.is_empty() {
            return Some(true);
        }
        // Web menus (Electron, Chromium) carry aria-checked as the item's value.
        return match n.value.as_str() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => Some(false),
        };
    }
    if matches!(n.role.as_str(), "AXCheckBox" | "AXRadioButton") || n.subrole == "AXSwitch" {
        return match n.value.as_str() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        };
    }
    None
}
fn state_words(n: &Node) -> String {
    let mut parts = vec![];
    if let Some(c) = checked(n) {
        parts.push(if n.role == "AXMenuItem" { if c { "ticked" } else { "not ticked" } } else if c { "on" } else { "off" }.to_owned());
    } else if text_role(n) && !secure(n) && !labels(n).iter().any(|l| payment_field(l)) {
        parts.push(format!("text “{}”", tail(&n.value, 80)));
    }
    if !n.enabled {
        parts.push("disabled".into());
    }
    parts.join(", ")
}
fn tail(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n { s.to_owned() } else { format!("…{}", s.chars().skip(count - n).collect::<String>()) }
}
fn at(n: &Node) -> String {
    n.frame.map(|(x, y, w, h)| format!(" at {:.0},{:.0} ({:.0}×{:.0})", x + w / 2.0, y + h / 2.0, w, h)).unwrap_or_default()
}
fn place(flat: &[Flat], i: usize) -> String {
    match container(flat, i).filter(|c| *c != i) {
        Some(c) if flat[c].node.role == "AXMenu" => " in the open menu".into(),
        Some(c) if !label(flat[c].node).is_empty() => format!(" in “{}”", label(flat[c].node)),
        _ => String::new(),
    }
}
fn visible(n: &Node) -> bool {
    n.frame.is_none_or(|(_, _, w, h)| w > 0.0 && h > 0.0)
}

/// Similarity for "closest matches": shared words first, then edit distance.
fn closeness(query: &str, label: &str) -> f64 {
    let (q, l) = (norm(query), norm(label));
    if q.is_empty() || l.is_empty() {
        return 0.0;
    }
    if l.contains(&q) || q.contains(&l) {
        return 0.9;
    }
    let (qa, la): (Vec<char>, Vec<char>) = (q.chars().collect(), l.chars().collect());
    let mut prev: Vec<usize> = (0..=la.len()).collect();
    for (i, qc) in qa.iter().enumerate() {
        let mut cur = vec![i + 1; la.len() + 1];
        for (j, lc) in la.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(qc != lc)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    1.0 - prev[la.len()] as f64 / qa.len().max(la.len()) as f64
}

// ---- the tool ----

pub struct Computer<'a, B: Backend> {
    pub backend: &'a B,
    pub gate: Gate,
    /// The user's recent lines (the transcript's, newest last): what `owner_asked` must quote (review C3).
    pub owner_lines: &'a [String],
}

struct Target {
    app: AppInfo,
    snapshot: Snapshot,
    /// The action writes or clicks: a cut tree refuses it.
    strict: bool,
}

impl<B: Backend> Computer<'_, B> {
    /// Runs one call and answers in one or a few plain lines.
    pub fn run(&self, r: &Request) -> Result<String> {
        if !self.gate.enabled {
            bail!("COMPUTER_OFF: controlling apps is off. {{user}} turns it on in Heimdall's menu (Control Apps), or you set settings key computer_control to true only when they ask for it.");
        }
        if !self.gate.seeing && r.action == Action::Find {
            bail!("{SEEING_OFF}");
        }
        if !self.backend.trusted() {
            bail!("ACCESSIBILITY_MISSING: Heimdall does not hold the Accessibility permission. {{user}} allows Heimdall in System Settings, Privacy & Security, Accessibility; turning Control Apps off and on asks macOS again.");
        }
        let app = self.app(&r.app)?;
        if let Some(why) = refused_app(&app) {
            bail!("REFUSED: {} is {why}; Heimdall never controls it. Ask {{user}} to do it himself.", app.name);
        }
        let snapshot = self.backend.snapshot(app.pid)?;
        let strict = matches!(r.action, Action::Set | Action::Type | Action::Click);
        let t = Target { app, snapshot, strict };
        match r.action {
            Action::Find => self.find(&t, r),
            Action::Press => self.press(&t, r),
            Action::Menu => self.menu(&t, r),
            Action::Pick => self.pick(&t, r),
            Action::Set => self.set(&t, r),
            Action::Type => self.type_text(&t, r),
            Action::Click => self.click(&t, r),
        }
    }

    fn app(&self, name: &str) -> Result<AppInfo> {
        let apps = self.backend.apps();
        let q = norm(name);
        let exact = apps.iter().find(|a| norm(&a.name) == q || a.bundle_id.eq_ignore_ascii_case(name.trim()));
        if let Some(a) = exact {
            return Ok(a.clone());
        }
        let starts: Vec<_> = apps.iter().filter(|a| norm(&a.name).starts_with(&q)).collect();
        if starts.len() == 1 {
            return Ok(starts[0].clone());
        }
        let mut names: Vec<_> = apps.iter().filter(|a| refused_app(a).is_none()).map(|a| a.name.clone()).collect();
        names.sort_by(|a, b| closeness(name, b).total_cmp(&closeness(name, a)));
        names.truncate(12);
        if starts.len() > 1 {
            bail!("APP_AMBIGUOUS: several running apps start with “{name}”: {}. Pass the full name.", starts.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "));
        }
        bail!("APP_NOT_FOUND: no running app named “{name}”. Running: {}. Open it first (open -a), then call again.", names.join(", "))
    }

    /// Elements matching role and title, best score only, each moved to the nearest element (itself or an ancestor
    /// within four levels) that has `action`, deduplicated. Returns indexes into `flat`.
    fn candidates(&self, flat: &[Flat], r: &Request, action: Option<&str>, only_menus: bool) -> Result<Vec<usize>> {
        let roles = r.role.as_deref().map(role_set).transpose()?;
        let scored: Vec<(usize, u8)> = flat
            .iter()
            .enumerate()
            .filter(|(i, f)| f.depth > 0 && role_matches(f.node, &roles) && (!only_menus || in_menu(flat, *i)))
            .map(|(i, f)| (i, r.title.as_deref().map_or(3, |q| score(f.node, q))))
            .filter(|(_, s)| *s > 0)
            .collect();
        let best = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
        let mut out: Vec<usize> = vec![];
        for (i, s) in scored {
            if s != best {
                continue;
            }
            let j = match action {
                Some(a) => std::iter::once(i)
                    .chain(ancestors(flat, i).take(4))
                    .find(|k| self.backend.actions(flat[*k].node.handle).iter().any(|x| x == a))
                    .unwrap_or(i),
                None => i,
            };
            if !out.contains(&j) {
                out.push(j);
            }
        }
        // An element inside another match is the same thing (a row and its text).
        let all = out.clone();
        out.retain(|i| !ancestors(flat, *i).any(|a| all.contains(&a)));
        // On screen first; hidden ones only when nothing else matches.
        if out.iter().any(|i| visible(flat[*i].node)) {
            out.retain(|i| visible(flat[*i].node));
        }
        Ok(out)
    }

    /// Exactly one element for an action, or an error that lists the candidates or the closest names.
    fn one(&self, t: &Target, flat: &[Flat], r: &Request, action: Option<&str>, only_menus: bool) -> Result<usize> {
        let found = self.candidates(flat, r, action, only_menus)?;
        let what = match (&r.role, &r.title) {
            (Some(role), Some(title)) => format!("{role} “{title}”"),
            (None, Some(title)) => format!("“{title}”"),
            (Some(role), None) => role.clone(),
            (None, None) => "element".into(),
        };
        let where_ = if only_menus { "an open menu of ".to_owned() } else { String::new() };
        match (found.len(), r.index) {
            (0, _) => bail!("{}", self.not_found(t, flat, r, &what, &where_, only_menus)),
            (n, Some(k)) if k == 0 || k > n => bail!("INVALID_ARGUMENT: index: {k}, but {n} match{}", if n == 1 { "es" } else { "" }),
            (_, Some(k)) => Ok(found[k - 1]),
            (1, None) => Ok(found[0]),
            (n, None) => {
                let mut s = format!("AMBIGUOUS: {n} elements match {what} in {}{}; pass index:", where_, t.app.name);
                for (k, i) in found.iter().take(LIST_MAX).enumerate() {
                    if self.gate.seeing {
                        let _ = write!(s, " {}. {}{}{};", k + 1, describe(flat[*i].node), place(flat, *i), at(flat[*i].node));
                    } else {
                        // Seeing off: kinds and places on screen only, no text (review C4).
                        let _ = write!(s, " {}. {}{};", k + 1, kind(flat[*i].node), at(flat[*i].node));
                    }
                }
                if n > LIST_MAX {
                    let _ = write!(s, " and {} more.", n - LIST_MAX);
                }
                Err(anyhow::anyhow!(s.trim_end_matches(';').to_owned()))
            }
        }
    }

    fn not_found(&self, t: &Target, flat: &[Flat], r: &Request, what: &str, where_: &str, only_menus: bool) -> String {
        let mut s = format!("NOT_FOUND: no {what} in {where_}{}'s windows", t.app.name);
        if only_menus && !flat.iter().any(|f| f.node.role == "AXMenu") {
            s.push_str("; no menu is open. Open one first with action menu");
        }
        if !self.gate.seeing {
            // Seeing off: no closest names, which would read other elements' text (review C4).
            s.push_str(". Seeing is off, so other names are not read; use the name {user} gave");
        } else if let Some(q) = &r.title {
            let mut close: Vec<(f64, usize)> = flat
                .iter()
                .enumerate()
                .filter(|(_, f)| f.depth > 0 && !label(f.node).is_empty())
                .map(|(i, f)| (labels(f.node).iter().map(|l| closeness(q, l)).fold(0.0, f64::max), i))
                .filter(|(c, _)| *c >= 0.4)
                .collect();
            close.sort_by(|a, b| b.0.total_cmp(&a.0));
            let mut seen = vec![];
            let names: Vec<String> = close
                .into_iter()
                .map(|(_, i)| describe(flat[i].node) + &place(flat, i))
                .filter(|d| if seen.contains(d) { false } else { seen.push(d.clone()); true })
                .take(5)
                .collect();
            if names.is_empty() {
                s.push_str(". Nothing named like it; call find with this app to see what is there");
            } else {
                let _ = write!(s, ". Closest: {}", names.join("; "));
                if r.role.is_some() {
                    s.push_str(" (drop role to match any kind)");
                }
            }
        }
        if t.snapshot.truncated {
            s.push_str(". The tree was too large to read whole; narrow with role");
        }
        s.push('.');
        s
    }

    fn check_target(&self, t: &Target, flat: &[Flat], i: usize) -> Result<()> {
        let n = flat[i].node;
        // A cut walk may have missed a password or card field in the window (review 10 Nov): writing and clicking need
        // the whole window seen.
        if t.strict && t.snapshot.truncated {
            bail!("TOO_LARGE: {}'s accessibility tree was too large to read whole, so its window could not be checked for password or payment fields; nothing was written. Close other windows or tabs of it and try again.", t.app.name);
        }
        if secure(n) {
            bail!("REFUSED: {} is a password field; Heimdall never fills or presses one. {{user}} does it himself.", describe(n));
        }
        if let Some(why) = refused_window(flat, i) {
            // Seeing off: the window's title is not read out (review C4).
            let why = if self.gate.seeing || !why.starts_with("its window “") { why } else { "its window is about passwords or payment".into() };
            bail!("REFUSED: {}: {why}. {{user}} does this himself.", describe(n));
        }
        if spends(&label(n)) || labels(n).iter().any(|l| spends(l)) {
            bail!("REFUSED: {} would buy or pay; {{user}} presses it themselves.", describe(n));
        }
        if !n.enabled {
            bail!("DISABLED: {} is disabled in its app right now; nothing was pressed.", describe(n));
        }
        Ok(())
    }

    /// An element that sends, posts, deletes, approves or installs is pressed only when the user's own recent words name
    /// its label (`owner_asked`). Checked on the element itself and on the pressable elements up to four levels above it
    /// (a click lands on a button's icon or text); an ancestor counts by its own labels, never a descendant's.
    fn check_outward(&self, flat: &[Flat], i: usize, r: &Request) -> Result<()> {
        let pressable = |n: &Node| {
            matches!(n.role.as_str(), "AXButton" | "AXLink" | "AXMenuItem" | "AXCheckBox" | "AXRadioButton" | "AXPopUpButton" | "AXMenuButton")
        };
        let mut chain = vec![(i, true)];
        chain.extend(ancestors(flat, i).take(4).filter(|k| pressable(flat[*k].node)).map(|k| (k, false)));
        for (k, own) in chain {
            let n = flat[k].node;
            let shown = label(n);
            let mut texts = labels(n);
            if own {
                texts.insert(0, shown.as_str());
            }
            let Some(what) = texts.into_iter().find_map(outward) else { continue };
            // An ancestor reaching here has labels of its own, so `shown` is its own first label.
            let name = shown;
            if !r.owner_asked.as_deref().is_some_and(|q| asks::names_label(q, self.owner_lines, &name)) {
                bail!(
                    "REFUSED: {} would {what} something; {{user}} presses it themselves unless their own words name it. When they asked for exactly this, pass owner_asked quoting their recent words with its label (“{name}”).",
                    describe(n)
                );
            }
        }
        Ok(())
    }

    fn find(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let listing = r.title.is_none() && r.role.is_none();
        let found: Vec<usize> = if listing {
            // A survey: what can be acted on, with a name.
            flat.iter()
                .enumerate()
                .filter(|(_, f)| {
                    f.depth > 0
                        && visible(f.node)
                        && !label(f.node).is_empty()
                        && matches!(f.node.role.as_str(), "AXButton" | "AXMenuItem" | "AXCheckBox" | "AXRadioButton" | "AXTextField" | "AXTextArea" | "AXSearchField" | "AXComboBox" | "AXLink" | "AXPopUpButton" | "AXMenuButton" | "AXSlider" | "AXRow" | "AXCell" | "AXWindow" | "AXMenu" | "AXTab")
                })
                .map(|(i, _)| i)
                .collect()
        } else {
            self.candidates(&flat, r, None, false)?
        };
        if found.is_empty() {
            let what = r.title.clone().map(|t| format!("“{t}”")).or(r.role.clone()).unwrap_or_else(|| "named control".into());
            return Err(anyhow::anyhow!(self.not_found(t, &flat, r, &what, "", false)));
        }
        let mut s = if listing {
            format!("{}: {} named controls{}.", t.app.name, found.len(), if t.app.active { "" } else { " (in the background)" })
        } else {
            format!("{}: {} match{}.", t.app.name, found.len(), if found.len() == 1 { "" } else { "es" })
        };
        for (k, i) in found.iter().take(FIND_LINES).enumerate() {
            let n = flat[*i].node;
            let state = state_words(n);
            let refused = if secure(n) {
                " (password field: refused)"
            } else if spends(&label(n)) {
                " (buys or pays: refused)"
            } else if outward(&label(n)).is_some() {
                " (refused unless {user}'s words name it)"
            } else {
                ""
            };
            let _ = write!(s, "\n{}. {}{}{}{}{refused}", k + 1, describe(n), if state.is_empty() { String::new() } else { format!(", {state}") }, place(&flat, *i), at(n));
        }
        if found.len() > FIND_LINES {
            let _ = write!(s, "\n… {} more; narrow with role or title.", found.len() - FIND_LINES);
        }
        if t.snapshot.truncated {
            s.push_str("\nThe tree was too large to read whole; some elements may be missing.");
        }
        Ok(s)
    }

    fn press(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let i = self.one(t, &flat, r, Some("AXPress"), false)?;
        self.check_target(t, &flat, i)?;
        self.check_outward(&flat, i, r)?;
        let n = flat[i].node;
        let acts = self.backend.actions(n.handle);
        if !acts.iter().any(|a| a == "AXPress") {
            return self.click_element(t, &flat, i, false, &acts);
        }
        if let Some(line) = unconfirmed(self.backend.perform(n.handle, "AXPress"), n, &t.app.name)? {
            return Ok(line);
        }
        self.backend.settle(250);
        Ok(match self.backend.reread(n.handle) {
            Some(after) => {
                let state = state_words(&after);
                let before = state_words(n);
                if state.is_empty() {
                    format!("Pressed {} in {}.", describe(n), t.app.name)
                } else if state == before {
                    format!("Pressed {} in {}; it still reads {state}.", describe(n), t.app.name)
                } else {
                    format!("Pressed {} in {}; it now reads {state} (was {before}).", describe(n), t.app.name)
                }
            }
            None => format!("Pressed {} in {}; it no longer reads back (its window, sheet or menu closed, or the app did not answer).", describe(n), t.app.name),
        })
    }

    fn menu(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let i = self.one(t, &flat, r, Some("AXShowMenu"), false)?;
        self.check_target(t, &flat, i)?;
        let n = flat[i].node;
        let acts = self.backend.actions(n.handle);
        if !acts.iter().any(|a| a == "AXShowMenu") {
            return self.click_element(t, &flat, i, true, &acts);
        }
        let before: Vec<usize> = flat.iter().filter(|f| f.node.role == "AXMenu").map(|f| f.node.handle).collect();
        // A native menu blocks its app while open, so the call may time out after the menu is up.
        if let Err(e) = self.backend.perform(n.handle, "AXShowMenu") {
            if !e.to_string().contains("CANNOT_COMPLETE") {
                return Err(e);
            }
        }
        self.backend.settle(350);
        let after = self.backend.snapshot(t.app.pid)?;
        let aflat = flatten(&after.root);
        let menus: Vec<usize> = aflat.iter().enumerate().filter(|(_, f)| f.node.role == "AXMenu").map(|(k, _)| k).collect();
        let fresh: Vec<usize> = menus.iter().copied().filter(|k| !before.contains(&aflat[*k].node.handle)).collect();
        let shown = if fresh.is_empty() { menus } else { fresh };
        let Some(&m) = shown.last() else {
            return Ok(format!("Asked {} in {} for its menu; no open menu shows in its accessibility tree. Try press, or find to see what changed.", describe(n), t.app.name));
        };
        let items: Vec<String> = subtree(&aflat, m)
            .filter(|k| aflat[*k].node.role == "AXMenuItem" && !label(aflat[*k].node).is_empty())
            .map(|k| {
                let it = aflat[k].node;
                let mut s = label(it);
                if checked(it) == Some(true) {
                    s.push_str(" (ticked)");
                }
                if !it.enabled {
                    s.push_str(" (disabled)");
                }
                s
            })
            .collect();
        if !self.gate.seeing {
            // Seeing off: the menu's items are other elements' text (review C4).
            return Ok(format!(
                "Opened the menu of {} in {} ({} items; Seeing is off, so they are not read). Pick the one {{user}} named with action pick; it stays open until then.",
                describe(n),
                t.app.name,
                items.len()
            ));
        }
        Ok(format!(
            "Opened the menu of {} in {}: {}. Pick one with action pick and its title; it stays open until then.",
            describe(n),
            t.app.name,
            if items.is_empty() { "no named items".to_owned() } else { items.join(", ") }
        ))
    }

    fn pick(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let i = self.one(t, &flat, r, None, true)?;
        // The item itself, or the nearest pressable one around it.
        let i = std::iter::once(i)
            .chain(ancestors(&flat, i).take(3))
            .find(|k| self.backend.actions(flat[*k].node.handle).iter().any(|a| a == "AXPress" || a == "AXPick"))
            .unwrap_or(i);
        self.check_target(t, &flat, i)?;
        self.check_outward(&flat, i, r)?;
        let n = flat[i].node;
        let acts = self.backend.actions(n.handle);
        let action = if acts.iter().any(|a| a == "AXPress") { "AXPress" } else if acts.iter().any(|a| a == "AXPick") { "AXPick" } else {
            return self.click_element(t, &flat, i, false, &acts);
        };
        let before = checked(n);
        if let Some(line) = unconfirmed(self.backend.perform(n.handle, action), n, &t.app.name)? {
            return Ok(line);
        }
        // A native menu fades out after the pick and its item reads back unchanged meanwhile (found in the real test,
        // 10 Nov), and its items outlive it: whether it is open is read from the app's tree, again, until it closes or
        // the item's state changes, for at most about 1.2 s.
        let mut open = Some(true);
        for _ in 0..8 {
            self.backend.settle(150);
            open = self.backend.snapshot(t.app.pid).ok().map(|s| flatten(&s.root).iter().any(|f| f.node.role == "AXMenu"));
            if open != Some(true) || self.backend.reread(n.handle).is_some_and(|a| checked(&a) != before) {
                break;
            }
        }
        let was = match before {
            Some(true) => " It was ticked before.",
            Some(false) => " It showed no tick before.",
            None => "",
        };
        if open.is_none() {
            return Ok(format!("Picked {} in {}; the app did not answer afterwards, so the result is unread: check with find.{was}", describe(n), t.app.name));
        }
        Ok(match self.backend.reread(n.handle).filter(|a| open == Some(true) && !a.role.is_empty()) {
            Some(after) => match checked(&after) {
                Some(c) => format!("Picked {} in {}; the menu is still open and it now reads {}.{was}", describe(n), t.app.name, if c { "ticked" } else { "not ticked" }),
                None => format!("Picked {} in {}; the menu is still open.", describe(n), t.app.name),
            },
            None => format!("Picked {} in {}; the menu closed.{was}", describe(n), t.app.name),
        })
    }

    fn set(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let i = self.one(t, &flat, r, None, false)?;
        self.check_target(t, &flat, i)?;
        // A switch's or checkbox's value acts as a press does (an "Allow" switch); a field's text sends nothing.
        if !text_role(flat[i].node) {
            self.check_outward(&flat, i, r)?;
        }
        let n = flat[i].node;
        let text = r.text.as_deref().unwrap_or_default();
        self.backend.set_text(n.handle, "AXValue", text)?;
        self.backend.settle(150);
        let now = self.backend.reread(n.handle).map(|a| a.value).unwrap_or_default();
        Ok(if now == text {
            format!("Set {} in {}; it now reads “{}”.", describe(n), t.app.name, tail(&now, 120))
        } else {
            format!("Sent “{}” to {} in {}, but it reads “{}” now: the app did not take it as written.", tail(text, 120), describe(n), t.app.name, tail(&now, 120))
        })
    }

    fn type_text(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        let text = r.text.as_deref().unwrap_or_default();
        let field = if r.title.is_some() || r.role.is_some() {
            let i = self.one(t, &flat, r, None, false)?;
            self.check_target(t, &flat, i)?;
            let n = flat[i].node.clone();
            if !text_role(&n) {
                bail!("NOT_A_FIELD: {} is not a text field; name the field (role field lists them).", describe(&n));
            }
            self.backend.focus(n.handle)?;
            n
        } else {
            let Some(n) = self.backend.focused(t.app.pid) else {
                bail!("NO_FOCUS: nothing has the keyboard focus in {}; name the field with title.", t.app.name);
            };
            if secure(&n) {
                bail!("REFUSED: the focused field in {} is a password field; {{user}} types it himself.", t.app.name);
            }
            if !text_role(&n) {
                bail!("NOT_A_FIELD: the focus in {} is on {}, not a text field; name the field with title.", t.app.name, describe(&n));
            }
            // The focused field's window is checked as a named one would be; a field the walk did not see is refused
            // rather than typed into unchecked (review 10 Nov).
            let Some(k) = flat.iter().position(|f| f.node.handle == n.handle) else {
                bail!("NOT_FOUND: the focused field in {} is not in its windows' tree, so its window cannot be checked; name the field with title.", t.app.name);
            };
            self.check_target(t, &flat, k)?;
            n
        };
        let before = self.backend.reread(field.handle).map(|a| a.value);
        // At the caret (replacing a selection) as typing would; a field that refuses that gets its text appended, only
        // when its text was read (an unread field would be wiped; review 10 Nov).
        let how = match self.backend.set_text(field.handle, "AXSelectedText", text) {
            Ok(()) => "at the caret",
            Err(e) => {
                let Some(before) = &before else {
                    bail!("NOT_TYPED: {} in {} takes no text at the caret and its text could not be read, so nothing was written ({e}).", describe(&field), t.app.name);
                };
                self.backend.set_text(field.handle, "AXValue", &format!("{before}{text}"))?;
                "at the end"
            }
        };
        self.backend.settle(150);
        let now = self.backend.reread(field.handle).map(|a| a.value).unwrap_or_default();
        // Read back only counts when the text changed and holds the typed words (it may have held them already).
        Ok(if now.contains(text) && before.as_deref() != Some(now.as_str()) {
            format!("Typed {} characters {how} in {} in {}; it now reads “{}”. Nothing was sent: no Return was pressed.", text.chars().count(), describe(&field), t.app.name, tail(&now, 120))
        } else {
            format!("Sent the text to {} in {}, but it does not read it back (it reads “{}”); the app may not accept typed text this way.", describe(&field), t.app.name, tail(&now, 120))
        })
    }

    fn click(&self, t: &Target, r: &Request) -> Result<String> {
        let flat = flatten(&t.snapshot.root);
        if r.point.is_none() {
            let i = self.one(t, &flat, r, None, false)?;
            self.check_target(t, &flat, i)?;
            self.check_outward(&flat, i, r)?;
            let acts = self.backend.actions(flat[i].node.handle);
            return self.click_element(t, &flat, i, r.right, &acts);
        }
        if !self.gate.clicks {
            bail!("CLICKS_OFF: pointer clicks are off ({{user}}'s decision); use press, menu or pick, which act through Accessibility.");
        }
        let (x, y) = r.point.unwrap();
        // The point must land in one of this app's windows or menus, and in none that is refused.
        let hit = flat
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.node.role.as_str(), "AXWindow" | "AXSheet" | "AXMenu"))
            .find(|(_, f)| f.node.frame.is_some_and(|(fx, fy, w, h)| x >= fx && y >= fy && x < fx + w && y < fy + h))
            .map(|(k, _)| k);
        let Some(w) = hit else {
            bail!("OUTSIDE: {x:.0},{y:.0} is not inside any window of {}; find gives each element's centre.", t.app.name);
        };
        // The deepest element under the point, for the receipt and the checks.
        let under = subtree(&flat, w)
            .filter(|k| flat[*k].node.frame.is_some_and(|(fx, fy, fw, fh)| x >= fx && y >= fy && x < fx + fw && y < fy + fh))
            .last()
            .unwrap_or(w);
        self.check_target(t, &flat, under)?;
        self.check_outward(&flat, under, r)?;
        self.pointer(t, x, y, r.right, flat[under].node)
    }

    /// The pointer fallback for an element Accessibility cannot act on.
    fn click_element(&self, t: &Target, flat: &[Flat], i: usize, right: bool, acts: &[String]) -> Result<String> {
        let n = flat[i].node;
        let Some((x, y, w, h)) = n.frame.filter(|_| visible(n)) else {
            bail!("NO_ACTION: {} has no way to be {} through Accessibility (its actions: {}) and no place on screen to click.", describe(n), if right { "opened" } else { "pressed" }, list_or_none(acts));
        };
        if !self.gate.clicks {
            bail!(
                "NO_ACTION: {} cannot be {} through Accessibility (its actions: {}), and pointer clicks are off. Pointer clicks are {{user}}'s decision; tell them this needs one.",
                describe(n),
                if right { "opened" } else { "pressed" },
                list_or_none(acts)
            );
        }
        self.pointer(t, x + w / 2.0, y + h / 2.0, right, n)
    }

    fn pointer(&self, t: &Target, x: f64, y: f64, right: bool, n: &Node) -> Result<String> {
        if !self.gate.clicks {
            bail!("CLICKS_OFF: pointer clicks are off ({{user}}'s decision); use press, menu or pick, which act through Accessibility.");
        }
        let idle = self.backend.idle_seconds();
        if idle < POINTER_IDLE_S {
            bail!("BUSY: {{user}} touched the mouse or keyboard {idle:.1} s ago; a click waits until they have been still for {POINTER_IDLE_S:.0} s. Try again in a moment.");
        }
        self.backend.click(t.app.pid, x, y, right)?;
        self.backend.settle(250);
        Ok(format!(
            "{} {} in {} at {x:.0},{y:.0}, sent to the app only; the pointer did not move. A click cannot be read back: check with find.",
            if right { "Right-clicked" } else { "Clicked" },
            describe(n),
            t.app.name
        ))
    }
}
/// An action the app did not confirm in time was usually delivered (it blocked on a menu or alert): it is answered as
/// sent, never as a failure the agent would retry and so toggle twice (review 10 Nov).
fn unconfirmed(result: Result<()>, n: &Node, app: &str) -> Result<Option<String>> {
    match result {
        Ok(()) => Ok(None),
        Err(e) if e.to_string().contains("CANNOT_COMPLETE") => Ok(Some(format!(
            "Sent to {} in {app}, but the app did not confirm in time (it may be showing a menu or alert). Check with find before trying again: a second press could undo it.",
            describe(n)
        ))),
        Err(e) => Err(e),
    }
}
fn list_or_none(acts: &[String]) -> String {
    if acts.is_empty() { "none".into() } else { acts.join(", ") }
}

/// One call on the real Mac: the gate as it is now, Seeing as the app holds it, and the user's recent lines (newest last)
/// for `owner_asked`.
#[cfg(target_os = "macos")]
pub fn run(args: &Value, owner_lines: &[String], seeing: bool) -> Result<String> {
    let request = Request::from_args(args)?;
    let gate = gate::now(seeing);
    // The off answer needs no Accessibility call at all.
    if !gate.enabled {
        return Computer { backend: &fake::Fake::empty(), gate, owner_lines }.run(&request);
    }
    let backend = mac::Mac::new();
    Computer { backend: &backend, gate, owner_lines }.run(&request)
}
#[cfg(not(target_os = "macos"))]
pub fn run(_: &Value, _: &[String], _: bool) -> Result<String> {
    bail!("UNAVAILABLE: controlling apps needs macOS")
}

/// Heimdall holds Accessibility; never prompts.
pub fn trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
/// Asks macOS for Accessibility when it is not granted (its own prompt, once). Called only when the user turns Control
/// Apps on.
pub fn request_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::request()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(test)]
mod tests;
