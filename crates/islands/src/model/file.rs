//! The data folder and the island files: atomic writes, the folder lock every read-modify-write holds, responses.

use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{j, time, Json};

pub struct Paths;

impl Paths {
    /// FLOATING_ISLANDS_DIR overrides the root (lab runs use an isolated folder).
    pub fn root() -> PathBuf {
        if let Ok(d) = std::env::var("FLOATING_ISLANDS_DIR") {
            if !d.is_empty() {
                return PathBuf::from(file_url_path(&expand_tilde(&d)));
            }
        }
        home().join("Library/Application Support/FloatingIslands")
    }
    pub fn islands() -> PathBuf {
        Self::root().join("islands")
    }
    pub fn responses() -> PathBuf {
        Self::root().join("responses")
    }
    /// What is on screen now, written by the app: per island open or not, frame, automatic or placed.
    pub fn state() -> PathBuf {
        Self::root().join("state.json")
    }
    /// Whether the islands show in screen shares and streams (Heimdall's Show Islands on Streams, `Streams`).
    pub fn streams() -> PathBuf {
        Self::root().join("streams.json")
    }
    pub fn pid() -> PathBuf {
        Self::root().join(".app.pid")
    }
    /// Whether every island is off screen at once (Heimdall's ⌃⌘, and Show Islands, `islands hide-all`).
    pub fn visibility() -> PathBuf {
        Self::root().join("visibility.json")
    }

    pub fn ensure() {
        for u in [Self::islands(), Self::responses()] {
            let _ = fs::create_dir_all(u);
        }
    }

    /// Letters, digits, `-`, `_` and `.` stay; everything else becomes `-` (an id never leaves the folder).
    pub fn safe_id(id: &str) -> String {
        id.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect()
    }

    pub fn file(id: &str) -> PathBuf {
        Self::islands().join(format!("{}.json", Self::safe_id(id)))
    }
}

/// `NSString.lastPathComponent` of a standardized absolute path.
pub fn last_path_component(p: &str) -> String {
    if p == "/" {
        return "/".into();
    }
    p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_owned()
}

/// `NSString.deletingPathExtension` of one path component: the part after the last `.` goes, unless that dot starts
/// or ends the name.
pub fn deleting_path_extension(name: &str) -> String {
    match name.rfind('.') {
        Some(k) if k > 0 && k + 1 < name.len() => name[..k].to_owned(),
        _ => name.to_owned(),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `NSString.expandingTildeInPath` for the current user (`~` and `~/…`).
pub fn expand_tilde(p: &str) -> String {
    if p == "~" {
        return home().to_string_lossy().into_owned();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest).to_string_lossy().into_owned();
    }
    p.to_owned()
}

/// `URL(fileURLWithPath:).path`: an absolute path keeps its `.` and `..` but loses repeated and trailing slashes (as
/// `NSString` path handling does); a relative one is resolved against the working directory with its dot segments
/// removed.
pub fn file_url_path(p: &str) -> String {
    if !p.starts_with('/') {
        let cwd = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
        return standardize(&format!("{cwd}/{p}"));
    }
    ns_path(p)
}

/// `NSString` path normalisation (`expandingTildeInPath` and friends): repeated slashes collapse, a trailing slash goes.
pub fn ns_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// `URL.standardizedFileURL.path` for an absolute path: `.` and `..` resolved lexically, no trailing slash.
pub fn standardize(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    format!("/{}", out.join("/"))
}

/// All islands off screen at once ("sometimes they are slightly annoying"): `visibility.json`
/// holds `{"hidden": true}`. Missing or unreadable: shown. Hiding changes no island file and no island's state; the
/// app only orders its windows out, and in again where they were.
pub struct Visibility;

impl Visibility {
    pub fn hidden_in(o: Option<&Json>) -> bool {
        o.and_then(|o| j::boolean(o.get("hidden"))).unwrap_or(false)
    }
    pub fn hidden() -> bool {
        Self::hidden_in(IslandFile::read(&Paths::visibility()).as_ref())
    }
    pub fn set_hidden(hidden: bool) -> std::io::Result<()> {
        let _ = fs::create_dir_all(Paths::root());
        let mut o = Json::new();
        o.insert("hidden".into(), Value::Bool(hidden));
        IslandFile::write_atomic(&o, &Paths::visibility())
    }
}

pub struct IslandFile;

thread_local! {
    static RESPONSE_COUNT: Cell<u64> = const { Cell::new(0) };
}

impl IslandFile {
    pub fn read(path: &Path) -> Option<Json> {
        let d = fs::read(path).ok()?;
        match serde_json::from_slice::<Value>(&d).ok()? {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Pretty, sorted keys, slashes unescaped (JSONSerialization's `.prettyPrinted, .sortedKeys, .withoutEscapingSlashes`),
    /// byte-for-byte what the Swift build writes.
    pub fn encode(o: &Json) -> Vec<u8> {
        super::jsonfmt::to_string(&Value::Object(o.clone()), true).into_bytes()
    }

    /// Write via a hidden temp file and rename, so a watcher never sees a half-written island.
    pub fn write_atomic(o: &Json, path: &Path) -> std::io::Result<()> {
        let data = Self::encode(o);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&data)?;
        }
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }

    /// Top-level merge; `null` in `over` removes the key.
    pub fn merge(base: &Json, over: &Json) -> Json {
        let mut out = base.clone();
        for (k, v) in over {
            if v.is_null() {
                out.remove(k);
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Every read-modify-write (the app's and the CLI's) holds one folder lock, so a drop and an agent's update never
    /// overwrite each other.
    pub fn locked<T>(body: impl FnOnce() -> T) -> T {
        Paths::ensure();
        let f = fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(Paths::islands().join(".write.lock"));
        let fd = f.as_ref().map(|f| f.as_raw_fd()).unwrap_or(-1);
        if fd >= 0 {
            // SAFETY: flock on a descriptor we own for the duration of `body`.
            unsafe { libc::flock(fd, libc::LOCK_EX) };
        }
        let out = body();
        if fd >= 0 {
            // SAFETY: as above.
            unsafe { libc::flock(fd, libc::LOCK_UN) };
        }
        drop(f);
        out
    }

    /// Show `abs` (an absolute, existing path) in an open viewer island: `view-<name>` unless `id` is given, titled by
    /// the name without its extension unless `title` is. Non-empty `actions` replace the proposed actions; empty keeps
    /// what is there. Returns the island's id and the written object (`islands view`, an open button on a text file).
    pub fn view(abs: &str, id: Option<String>, title: Option<String>, actions: Vec<Value>) -> (String, Json) {
        Self::view_from(abs, id, title, actions, None)
    }

    /// `view`, opened by a click on an island: `origin` (`FrameOrigin::json`) makes the viewer grow out of the clicked
    /// element instead of the notch. Without one, any origin a reused viewer still carries goes.
    pub fn view_from(abs: &str, id: Option<String>, title: Option<String>, actions: Vec<Value>, origin: Option<Value>) -> (String, Json) {
        let name = last_path_component(abs);
        let id = id.unwrap_or_else(|| format!("view-{}", Paths::safe_id(&name)));
        let title = title.unwrap_or_else(|| deleting_path_extension(&name));
        let o = Self::locked(|| {
            let old = Self::read(&Paths::file(&id)).unwrap_or_default();
            let mut o = Self::merge(
                &old,
                &j::object([
                    ("id", Value::from(id.clone())),
                    ("preset", Value::from("viewer")),
                    ("path", Value::from(abs.to_owned())),
                    ("title", Value::from(title)),
                    ("expanded", Value::Bool(true)),
                    ("hidden", Value::Bool(false)),
                ]),
            );
            if !actions.is_empty() {
                o.insert("actions".into(), Value::Array(actions));
            }
            match origin {
                Some(v) => {
                    o.insert("origin".into(), v);
                }
                None => {
                    o.remove("origin");
                }
            }
            let _ = Self::write_atomic(&o, &Paths::file(&id));
            o
        });
        (id, o)
    }

    /// A plan list as a writer must start from (call holding the lock; `PlanList::start_from`): the file, else its
    /// closed copy (hidden), else its kept copy, else fresh. `None` when the file exists but does not parse: it is never
    /// replaced, the write waits for a readable one.
    pub fn plan_start(list: &str) -> Option<Json> {
        let path = Paths::file(list);
        let file = Self::read(&path);
        if file.is_none() && path.exists() {
            return None;
        }
        let copy = |name: String| Self::read(&Paths::islands().join(name));
        Some(super::PlanList::start_from(list, file, copy(super::PlanList::closed_copy(list)), copy(super::PlanList::kept_copy(list))))
    }

    /// Write one island file (call holding the lock); a plan list (`PlanList::ALL`) also refreshes its kept copy
    /// `.kept-<list>.json`, as Heimdall's writers do (`goals::ops::write_list`), so a list deleted later comes back with
    /// what was written last (the newest asks, a toggled goal).
    /// A Goals file without goals never replaces a kept copy that has some (re-review 31 Oct: a goals-less write
    /// overwrote the only copy of the goals).
    pub fn write_island(id: &str, o: &Json) -> std::io::Result<()> {
        Self::write_atomic(o, &Paths::file(id))?;
        if super::PlanList::ALL.contains(&id) {
            let kept = Paths::islands().join(super::PlanList::kept_copy(id));
            let has_goals = |o: &Json| super::j::arr(o.get("goals")).is_some_and(|g| !g.is_empty());
            if id == "goals" && !has_goals(o) && Self::read(&kept).is_some_and(|k| has_goals(&k)) {
                return Ok(());
            }
            Self::write_atomic(o, &kept)?;
        }
        Ok(())
    }

    /// Read-modify-write one island file. Returns the written object, `None` if the island does not exist. A plan
    /// list's kept copy follows (`write_island`).
    pub fn patch(id: &str, change: impl FnOnce(&mut Json)) -> Option<Json> {
        Self::locked(|| {
            let url = Paths::file(id);
            let mut o = Self::read(&url)?;
            change(&mut o);
            let _ = Self::write_island(id, &o);
            Some(o)
        })
    }

    /// Record a ready response object (`Responses::tick`, `Responses::approval`); `island` names the file.
    pub fn respond_object(o: &Json) -> Option<PathBuf> {
        let island = j::str(o.get("island")).unwrap_or("island").to_owned();
        let kind = j::str(o.get("kind")).unwrap_or("button").to_owned();
        let mut extra = o.clone();
        extra.remove("island");
        extra.remove("kind");
        Self::respond_with(&island, &kind, extra)
    }

    /// Record a click (button, list row, toggle, slider) for the agent to pick up.
    pub fn respond(island: &str, kind: &str, action: Option<&str>, value: Option<Value>) -> Option<PathBuf> {
        let mut extra = Json::new();
        if let Some(a) = action {
            extra.insert("action".into(), Value::from(a));
        }
        if let Some(v) = value {
            extra.insert("value".into(), v);
        }
        Self::respond_with(island, kind, extra)
    }

    fn respond_with(island: &str, kind: &str, extra: Json) -> Option<PathBuf> {
        Paths::ensure();
        let now = time::now();
        let ms = (now * 1000.0) as i64;
        let mut o = Json::new();
        o.insert("island".into(), Value::from(island));
        o.insert("kind".into(), Value::from(kind));
        o.insert("at".into(), Value::from(time::iso8601(now)));
        for (k, v) in extra {
            if !v.is_null() {
                o.insert(k, v);
            }
        }
        let n = RESPONSE_COUNT.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        let url = Paths::responses().join(format!("{}-{ms}-{}-{n}.json", Paths::safe_id(island), std::process::id()));
        Self::write_atomic(&o, &url).ok().map(|_| url)
    }
}

/// Heimdall's Show Islands on Streams: `streams.json` holds `{"shown": true}` while the
/// islands appear in screen shares, streams and screenshots. Missing or unreadable: they do not, the default.
pub struct Streams;

impl Streams {
    pub fn shown_in(o: Option<&Json>) -> bool {
        o.and_then(|o| j::boolean(o.get("shown"))).unwrap_or(false)
    }
    pub fn shown() -> bool {
        Self::shown_in(IslandFile::read(&Paths::streams()).as_ref())
    }
    pub fn set_shown(shown: bool) -> std::io::Result<()> {
        let _ = fs::create_dir_all(Paths::root());
        let mut o = Json::new();
        o.insert("shown".into(), Value::Bool(shown));
        IslandFile::write_atomic(&o, &Paths::streams())
    }
}
