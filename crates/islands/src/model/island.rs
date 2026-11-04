//! Typed view of one island file. Unknown keys are kept in `raw` and survive every rewrite.

use std::path::Path;

use serde_json::Value;

use super::file::{expand_tilde, standardize};
use super::plan::{Plan, PlanItem, PlanList};
use super::viewer_kind::ViewerKind;
use super::{format_number, j, time, Json};

pub struct Presets;
impl Presets {
    pub const ALL: [&'static str; 7] = ["task", "waiting", "result", "list", "note", "viewer", "todo"];
    pub const STATES: [&'static str; 4] = ["running", "waiting", "done", "failed"];
}

/// The three stacks an island can dock to: a column at the left or right edge, or a row under the notch.
pub struct Dock;
impl Dock {
    pub const SIDES: [&'static str; 3] = ["left", "right", "top"];
}

/// A To do item's group, in display order, with the header the user reads.
pub struct TodoGroup;
impl TodoGroup {
    pub const ALL: [&'static str; 3] = ["running", "waiting", "discussed"];

    pub fn label(g: &str) -> &'static str {
        match g {
            "running" => "Running",
            "waiting" => "Waiting on you",
            _ => "Discussed",
        }
    }

    pub fn valid(g: Option<&str>) -> Option<String> {
        g.filter(|g| Self::ALL.contains(g)).map(str::to_owned)
    }

    /// "Echo bug=waiting" → ("Echo bug", Some("waiting"), false); "=done" ticks it; anything else stays text.
    pub fn parse(s: &str) -> (String, Option<String>, bool) {
        if let Some(eq) = s.rfind('=') {
            let tail = s[eq + 1..].trim().to_lowercase();
            let head = s[..eq].trim().to_owned();
            if Self::ALL.contains(&tail.as_str()) {
                return (head, Some(tail), false);
            }
            if tail == "done" {
                return (head, None, true);
            }
        }
        (s.trim().to_owned(), None, false)
    }
}

pub struct Step;
impl Step {
    pub const MARKS: [&'static str; 4] = ["done", "now", "failed", "todo"];

    pub fn mark(s: Option<&str>) -> &'static str {
        match s.map(str::to_lowercase).as_deref() {
            Some("done" | "ok" | "finished") => "done",
            Some("now" | "current" | "running" | "active") => "now",
            Some("failed" | "error") => "failed",
            _ => "todo",
        }
    }

    /// "Download=done" → ("Download", "done"); a suffix that is not a mark stays part of the title.
    pub fn parse(s: &str) -> (String, &'static str) {
        if let Some(eq) = s.rfind('=') {
            let tail = s[eq + 1..].trim().to_lowercase();
            if ["done", "ok", "finished", "now", "current", "running", "active", "failed", "error", "todo"].contains(&tail.as_str()) {
                return (s[..eq].trim().to_owned(), Self::mark(Some(&tail)));
            }
        }
        (s.trim().to_owned(), "todo")
    }
}

/// An open target: a URL with a scheme, an absolute or `~` path, or a bare domain (https).
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    File(String),
    Url(String),
}

/// Where a viewer opened by a click starts (`Island::born_from_frame`).
#[derive(Clone, Debug, PartialEq)]
pub struct FrameOrigin {
    pub rect: super::geometry::Rect,
    pub image: Option<String>,
    pub island: Option<String>,
}

impl FrameOrigin {
    /// The island file's `origin` value.
    pub fn json(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("from".into(), Value::from("frame"));
        for (k, v) in [("x", self.rect.x), ("y", self.rect.y), ("w", self.rect.w), ("h", self.rect.h)] {
            o.insert(k.into(), Value::from((v * 10.0).round() / 10.0));
        }
        if let Some(i) = &self.image {
            o.insert("image".into(), Value::from(i.clone()));
        }
        if let Some(i) = &self.island {
            o.insert("island".into(), Value::from(i.clone()));
        }
        Value::Object(o)
    }
}

/// What a viewer does with its `origin` frame when it is first placed: grow out of it only when it arrived while the
/// app runs (a click just now); one still in its file at launch is stale. Returns the frame to grow from, and whether
/// the key must go from the file now (it always goes once read: the 0.7 s cleanup after a grow, or at once).
pub fn frame_on_arrival(i: &Island, arrived_while_running: bool) -> (Option<FrameOrigin>, bool) {
    match i.born_from_frame() {
        Some(f) if arrived_while_running => (Some(f), false),
        Some(_) => (None, true),
        None => (None, false),
    }
}

/// A viewer already on screen whose file gained a frame origin (opened again by a click): the island it now comes
/// from; the key goes from the file at once (review, 30 Oct: it stayed, and a relaunch grew the viewer from it).
pub fn frame_on_update(new: &Island, placed: bool) -> Option<FrameOrigin> {
    new.born_from_frame().filter(|_| placed)
}

/// What a click on an `image` part opens: its `open` target, else the picture itself (a picture
/// without `open` was not clickable; clicking the picture opens it full size).
pub fn image_part_target(part: &Json) -> Option<String> {
    let s = |k: &str| j::str(part.get(k)).map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned);
    s("open").or_else(|| s("path")).or_else(|| s("src"))
}

/// An element's frame on screen (points, y up, as windows are placed): `body` in the island's body coordinates, the
/// island's `shape` in its view (y down, top-left origin), the window's frame on screen, and the body's scroll.
pub fn body_rect_on_screen(body: super::geometry::Rect, shape: super::geometry::Rect, window: super::geometry::Rect, pad: f64, body_top: f64, scroll: f64) -> super::geometry::Rect {
    let vx = shape.x + pad + body.x;
    let vy = shape.y + body_top - scroll + body.y;
    super::geometry::Rect::new(window.x + vx, window.y + window.h - vy - body.h, body.w, body.h)
}

/// The request a click on an island element makes: where `target` opens, and for the viewer, the origin that makes it
/// grow out of the clicked element (`frame` on screen, the clicked `image` for its first frame, the island it came
/// from). No frame: no origin (the viewer comes out of the notch, as one `islands_view` opens).
pub fn open_request(target: &str, island: &str, frame: Option<super::geometry::Rect>, image: Option<&str>) -> Option<(OpenRoute, Option<FrameOrigin>)> {
    let route = OpenTarget::route(target)?;
    let origin = match (&route, frame) {
        (OpenRoute::Viewer(_), Some(rect)) if rect.w > 0.0 && rect.h > 0.0 => {
            Some(FrameOrigin { rect, image: image.map(str::to_owned), island: Some(island.to_owned()) })
        }
        _ => None,
    };
    Some((route, origin))
}

/// File types a click must never hand to the system or show: apps and installers, scripts that run on a double click,
/// automations, links that open other apps, and other loadable code.
pub const LAUNCHABLE: [&str; 30] = ["app", "pkg", "mpkg", "dmg", "command", "tool", "terminal", "workflow", "action", "scpt", "scptd",
    "osax", "prefpane", "kext", "plugin", "bundle", "framework", "xpc", "appex", "qlgenerator", "mdimporter", "saver", "webloc", "inetloc",
    "fileloc", "url", "jar", "dylib", "so", "exe"];

/// `path` would run or launch something: a `LAUNCHABLE` type, or a file with an executable bit that the viewer would
/// not show as text (a script is shown as text, never run).
pub fn launchable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if LAUNCHABLE.contains(&ext.as_str()) {
        return true;
    }
    let exec = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    exec && !matches!(ViewerKind::detect(path), ViewerKind::Markdown | ViewerKind::Text(_))
}

/// Where an open button sends its target (`OpenTarget::route`).
#[derive(Clone, Debug, PartialEq)]
pub enum OpenRoute {
    /// An absolute path shown in a viewer island.
    Viewer(String),
    /// A folder, selected in a Finder window (never opened: a bundle would launch).
    Reveal(String),
    /// A web page, opened by NSWorkspace in the default browser.
    System(Target),
}

/// What an island's open button shows (`OpenTarget::button`): its words, whether the target is there (bright, with
/// an arrow) and whether a click reaches `open_target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenButton {
    pub label: String,
    pub ready: bool,
    pub live: bool,
}

pub struct OpenTarget;
impl OpenTarget {
    pub fn url(t: &str) -> Option<Target> {
        let t = t.trim();
        if t.is_empty() {
            return None;
        }
        if t.contains("://") || t.starts_with("mailto:") {
            if let Some(rest) = t.strip_prefix("file://") {
                return Some(Target::File(percent_decode(rest.strip_prefix("localhost").unwrap_or(rest))));
            }
            // Web pages only: another scheme (`x-apple.systempreferences:`, `ssh:`, an app's own) would launch an app.
            let web = t.starts_with("https://") || t.starts_with("http://");
            return (web && valid_url(t)).then(|| Target::Url(t.to_owned()));
        }
        // Files by absolute or `~/` path only (review, 30 Oct: a relative path resolved against the home folder
        // gave a prompt in a thread one more way to point a click at a file).
        if t.starts_with('/') || t.starts_with("~/") || t == "~" {
            return Some(Target::File(expand_tilde(t)));
        }
        if !t.contains(' ') && t.contains('.') {
            let u = format!("https://{t}");
            if host(&u).is_some() {
                return Some(Target::Url(u));
            }
        }
        None
    }

    /// What the button says: the file's name or the page's host.
    pub fn label(t: &str) -> String {
        match Self::url(t) {
            None => t.to_owned(),
            Some(Target::File(p)) => Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(p),
            Some(Target::Url(u)) => host(&u).unwrap_or_else(|| t.to_owned()).replace("www.", ""),
        }
    }

    /// A file target that is not there (the button shows it is missing instead of doing nothing).
    pub fn missing(t: &str) -> bool {
        match Self::url(t) {
            None => true,
            Some(Target::File(p)) => !Path::new(&p).exists(),
            Some(Target::Url(_)) => false,
        }
    }

    /// Where a click sends `t`: every file Heimdall's viewer island can show opens there (bug 24 Oct: a `.md` opened in
    /// Xcode; a picture opened in Preview; "every image, and every file a job island opens on click,
    /// must open in Heimdall's own floating viewer"): Markdown, text and code, pictures, PDFs, audio, video, HTML
    /// reports and any declared type QuickLook draws (`ViewerKind::shows`). Web pages (`http`) stay in the browser;
    /// folders, apps and files of no known type go to the system.
    ///
    /// A click never launches anything (review, 30 Oct: `open` on a folder bundle or an executable runs it): a folder
    /// (an app is one) is only revealed in Finder; a launchable type (`LAUNCHABLE`), an executable that is not text,
    /// or a file of no known type opens nowhere. Only web pages go to the system (the browser).
    pub fn route(t: &str) -> Option<OpenRoute> {
        match Self::url(t)? {
            Target::Url(u) => Some(OpenRoute::System(Target::Url(u))),
            Target::File(p) => {
                let path = Path::new(&p);
                let meta = std::fs::metadata(path).ok()?;
                if meta.is_dir() {
                    return Some(OpenRoute::Reveal(standardize(&p)));
                }
                (!launchable(path) && ViewerKind::shows(path)).then(|| OpenRoute::Viewer(standardize(&p)))
            }
        }
    }

    /// The open button for `t` on an island whose work is still `running` (or waiting). A file that is there: its name,
    /// live. A missing file while the work runs: the page to come (a task's island carries its report's button from
    /// the start), its name dimmed; a click opens it once it exists and does nothing before. A
    /// missing file once the work has settled: "Not found: name", no click.
    pub fn button(t: &str, running: bool) -> OpenButton {
        let label = Self::label(t);
        if !Self::missing(t) {
            return OpenButton { label, ready: true, live: true };
        }
        if running && Self::url(t).is_some() {
            return OpenButton { label, ready: false, live: true };
        }
        OpenButton { label: format!("Not found: {label}"), ready: false, live: false }
    }

    /// The string NSWorkspace opens.
    pub fn absolute_string(t: &Target) -> String {
        match t {
            Target::File(p) => format!("file://{}", percent_encode_path(p)),
            Target::Url(u) => u.clone(),
        }
    }
}

/// `URL(string:)` accepts it: no spaces or control characters.
fn valid_url(s: &str) -> bool {
    !s.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}'))
}

/// The host of a URL string (`scheme://[user@]host[:port]/…`).
pub fn host(u: &str) -> Option<String> {
    if !valid_url(u) {
        return None;
    }
    let rest = u.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let h = if auth.starts_with('[') { auth.split(']').next().map(|s| format!("{s}]"))? } else { auth.split(':').next()?.to_owned() };
    (!h.is_empty()).then_some(h)
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_path(p: &str) -> String {
    let mut out = String::new();
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One step of an island's sequence.
#[derive(Clone, Debug, PartialEq)]
pub struct StepRow {
    pub title: String,
    pub mark: &'static str,
}

/// An item of a plain or grouped list with its place in the file's `items`.
#[derive(Clone, Debug, PartialEq)]
pub struct TodoRow {
    pub index: usize,
    pub title: String,
    pub group: Option<String>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Island {
    pub id: String,
    pub raw: Json,
}

impl Island {
    pub fn new(id: &str, raw: Json) -> Island {
        Island { id: id.to_owned(), raw }
    }

    fn get(&self, k: &str) -> Option<&Value> {
        self.raw.get(k)
    }

    /// The plan islands are known by id (`goals`, `todo`, `scheduled`, `settled`): they are To do lists even without
    /// `preset`/`list`.
    pub fn preset(&self) -> &str {
        if PlanList::ALL.contains(&self.id.as_str()) {
            return "todo";
        }
        j::str(self.get("preset")).filter(|p| Presets::ALL.contains(p)).unwrap_or("task")
    }

    pub fn title(&self) -> String {
        if let Some(t) = j::str(self.get("title")) {
            return t.to_owned();
        }
        if self.preset() == "todo" {
            PlanList::title(self.list().as_deref().unwrap_or("todo")).to_owned()
        } else {
            self.id.clone()
        }
    }

    /// Which plan list a To do island is (`list`, else its id when it is a plan list's, else `todo`); `None` for every
    /// other preset.
    pub fn list(&self) -> Option<String> {
        if self.preset() != "todo" {
            return None;
        }
        if let Some(l) = j::str(self.get("list")).filter(|l| PlanList::ALL.contains(l)) {
            return Some(l.to_owned());
        }
        Some(if PlanList::ALL.contains(&self.id.as_str()) { self.id.clone() } else { "todo".into() })
    }

    pub fn plan_items(&self) -> Vec<PlanItem> {
        Plan::items(&self.raw)
    }

    pub fn state(&self) -> &str {
        if let Some(s) = j::str(self.get("state")).filter(|s| Presets::STATES.contains(s)) {
            return s;
        }
        match self.preset() {
            "waiting" => "waiting",
            "result" => "done",
            "task" => "running",
            _ => "done",
        }
    }

    pub fn progress(&self) -> Option<f64> {
        j::num(self.get("progress")).map(|p| p.clamp(0.0, 1.0))
    }

    pub fn value(&self) -> Option<String> {
        match self.get("value")? {
            Value::String(s) => Some(s.clone()),
            v => j::num(Some(v)).map(format_number),
        }
    }

    pub fn icon(&self) -> Option<String> {
        j::str(self.get("icon")).map(str::to_owned).or_else(|| self.list().map(|l| PlanList::icon(&l).to_owned()))
    }

    pub fn stage(&self) -> Option<String> {
        j::str(self.get("stage")).map(str::to_owned)
    }

    pub fn path(&self) -> Option<String> {
        j::str(self.get("path")).map(str::to_owned)
    }

    pub fn open(&self) -> Option<String> {
        j::str(self.get("open")).filter(|s| !s.trim().is_empty()).map(str::to_owned)
    }

    pub fn summary(&self) -> Option<String> {
        j::nonempty(self.get("summary")).map(str::to_owned)
    }

    pub fn image(&self) -> Option<String> {
        j::nonempty(self.get("image")).map(str::to_owned)
    }

    pub fn steps(&self) -> Vec<StepRow> {
        let Some(a) = j::arr(self.get("steps")) else { return vec![] };
        a.iter()
            .filter_map(|v| match v {
                Value::String(s) => {
                    let (title, mark) = Step::parse(s);
                    Some(StepRow { title, mark })
                }
                Value::Object(o) => {
                    let t = j::str(o.get("title"))?;
                    Some(StepRow { title: t.to_owned(), mark: Step::mark(j::str(o.get("state"))) })
                }
                _ => None,
            })
            .collect()
    }

    /// A plain list of items in order (no status marks), shown with bullets.
    pub fn items(&self) -> Vec<String> {
        self.todo_items().into_iter().map(|t| t.title).collect()
    }

    pub fn todo_items(&self) -> Vec<TodoRow> {
        let Some(a) = j::arr(self.get("items")) else { return vec![] };
        let mut out = vec![];
        for (k, v) in a.iter().enumerate() {
            match v {
                Value::String(s) => {
                    if !s.is_empty() {
                        out.push(TodoRow { index: k, title: s.clone(), group: None, done: false });
                    }
                }
                Value::Object(o) => {
                    let Some(t) = j::nonempty(o.get("title")) else { continue };
                    out.push(TodoRow {
                        index: k,
                        title: t.to_owned(),
                        group: TodoGroup::valid(j::str(o.get("group"))),
                        done: j::boolean(o.get("done")).unwrap_or(false),
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// Items under group headers: a To do island, or any list where an item names its group (ungrouped items count
    /// as discussed).
    pub fn grouped_items(&self) -> Option<Vec<(String, Vec<TodoRow>)>> {
        let all = self.todo_items();
        if self.preset() != "todo" && !all.iter().any(|t| t.group.is_some()) {
            return None;
        }
        let mut out = vec![];
        for g in TodoGroup::ALL {
            let rows: Vec<TodoRow> = all.iter().filter(|t| t.group.as_deref().unwrap_or("discussed") == g).cloned().collect();
            if !rows.is_empty() {
                out.push((g.to_owned(), rows));
            }
        }
        Some(out)
    }

    /// A To do item still open and waiting on the user: the island's symbol turns orange. On Goals, an ask that waits on
    /// him (not done, not answered, nobody else next); a goal never does.
    pub fn waits_on_owner(&self) -> bool {
        if self.list().as_deref() == Some("goals") {
            return !super::goals::Goals::waiting(&self.plan_items()).is_empty();
        }
        self.todo_items().iter().any(|t| t.group.as_deref() == Some("waiting") && !t.done)
    }

    /// A few key values in order.
    pub fn facts(&self) -> Vec<(String, String)> {
        let Some(a) = j::objs(self.get("kv")) else { return vec![] };
        a.iter()
            .filter_map(|o| {
                let l = j::str(o.get("label"))?;
                let v = match o.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    v => j::num(v).map(format_number).unwrap_or_default(),
                };
                Some((l.to_owned(), v))
            })
            .collect()
    }

    pub fn started(&self) -> Option<f64> {
        date(self.get("started"))
    }
    pub fn updated(&self) -> Option<f64> {
        date(self.get("updated"))
    }
    pub fn ended(&self) -> Option<f64> {
        date(self.get("ended"))
    }

    pub fn expanded(&self) -> bool {
        j::boolean(self.get("expanded")).unwrap_or(false)
    }
    pub fn hidden(&self) -> bool {
        j::boolean(self.get("hidden")).unwrap_or(false)
    }

    /// Place in its dock, lower first. Plan islands default to Goals 0, a left-over To do 0.5, Scheduled 1, Settled 2.
    pub fn order(&self) -> Option<f64> {
        j::num(self.get("order")).or_else(|| self.list().map(|l| PlanList::order(&l)))
    }

    pub fn width(&self) -> Option<f64> {
        j::num(self.get("width"))
    }

    pub fn chosen(&self) -> Option<String> {
        j::str(self.get("chosen")).map(str::to_owned)
    }

    pub fn parts(&self) -> Vec<Json> {
        j::objs(self.get("parts")).map(|v| v.into_iter().cloned().collect()).unwrap_or_default()
    }

    /// Placed freely: the top-left corner in points from the display's top-left (y down).
    pub fn position(&self) -> Option<(f64, f64)> {
        let p = j::obj(self.get("position"))?;
        Some((j::num(p.get("x"))?, j::num(p.get("y"))?))
    }

    /// The height the user pinned it at on its edge (its top from the display's top); `None`: it takes its turn.
    pub fn edge_y(&self) -> Option<f64> {
        if self.position().is_some() || self.side().as_deref() == Some("top") {
            return None;
        }
        j::num(self.get("edgeY"))
    }

    /// The dock it lives in when not placed freely. A To do list docks left, everything else right.
    pub fn side(&self) -> Option<String> {
        if self.position().is_some() {
            return None;
        }
        if let Some(s) = j::str(self.get("side")).filter(|s| Dock::SIDES.contains(s)) {
            return Some(s.to_owned());
        }
        Some(if self.preset() == "todo" { "left".into() } else { "right".into() })
    }

    /// `{"from": "frame", "x", "y", "w", "h", "image", "island"}`: a viewer opened by a click on an island grows out of
    /// the clicked element's frame (screen points, y up, as windows are placed), showing `image` (the clicked thumbnail)
    /// on its first frame; `island` is where it came from (closing shrinks back into it while it is on screen).
    pub fn born_from_frame(&self) -> Option<FrameOrigin> {
        let o = j::obj(self.get("origin"))?;
        if j::str(o.get("from")) != Some("frame") {
            return None;
        }
        let n = |k: &str| j::num(o.get(k));
        let (x, y, w, h) = (n("x")?, n("y")?, n("w")?, n("h")?);
        (w > 0.0 && h > 0.0).then(|| FrameOrigin {
            rect: super::geometry::Rect::new(x, y, w, h),
            image: j::str(o.get("image")).map(str::to_owned),
            island: j::str(o.get("island")).map(str::to_owned),
        })
    }

    /// `{"from": "notch"}`: it grows out of the notch.
    pub fn born_from_notch(&self) -> bool {
        j::obj(self.get("origin")).and_then(|o| j::str(o.get("from"))) == Some("notch")
    }

    /// The look follows the state: a task that is done shows as a result, one waiting shows as waiting.
    pub fn look(&self) -> &str {
        let (preset, state) = (self.preset(), self.state());
        if preset == "todo" {
            return "todo";
        }
        if state == "failed" {
            return "failed";
        }
        if state == "waiting" || preset == "waiting" {
            return if state == "done" { "result" } else { "waiting" };
        }
        if preset == "task" && state == "done" {
            return "result";
        }
        preset
    }

    /// A viewer's proposed actions (`actions`).
    pub fn viewer_actions(&self) -> Vec<super::ViewerAction> {
        super::ViewerAction::parse(self.get("actions"))
    }

    pub fn format(v: f64) -> String {
        format_number(v)
    }
}

/// An epoch-seconds number or an ISO 8601 string.
pub fn date(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::String(s) => time::parse_iso8601(s),
        v => j::num(Some(v)),
    }
}

/// The file a viewer `path` names: `file://` URLs and `~` are accepted; `.` and `..` are resolved.
pub fn viewer_path(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("file://") {
        return standardize(&percent_decode(rest.strip_prefix("localhost").unwrap_or(rest)));
    }
    let p = expand_tilde(raw);
    if p.starts_with('/') {
        standardize(&p)
    } else {
        let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
        standardize(&format!("{cwd}/{p}"))
    }
}
