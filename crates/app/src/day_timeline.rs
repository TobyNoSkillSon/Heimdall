//! Where the day went ("a floating island on the left side... during the day some agent is just
//! updating it. It's like a timeline of the day. What have we done today?"). Background Tasks ▸ Day Timeline.
//!
//! Every `PERIOD` a pass reads today from the archive, read-only: the screen states' `app` and `window_title` tags
//! and the final transcript lines with their `speaker` tags (`docs/chunk-index.md`). The day starts at
//! `DAY_START_HOUR` local, so work past midnight still counts as the evening it belongs to.
//! Deterministic first: the day is cut into 15-minute segments on a fixed grid; each gets its apps (minutes), window
//! titles, the people heard and a few lines of the conversation, and a kind guessed from them. A segment with no
//! screen and no speech is a gap. The background model (`background_model`, Claude Haiku 5.5 by default) only
//! groups the newest segments into blocks and names them: a title naming the project or activity and one line on
//! what got done. Blocks that ended more than `FREEZE_AFTER` ago are fixed and never sent again, so a pass sends
//! about the last hour and a half. A pass whose segments did not change makes no call. Without a model (no key,
//! a choice off Anthropic, Agent off) or on a bad answer the blocks come from the segments alone, named by app.
//! Each block also keeps what the row does not show: standard tags (`project:`, `activity:`, `app:`),
//! a few sentences on what happened, the people, threads and tasks, files and decisions. The tags give the guests
//! heard, the apps, T3 thread windows and file names in window titles on every pass; the model adds the rest for
//! the blocks it names. The island opens a row on a click to show them; `search` finds blocks across days.
//! Storage: `<data>/day-timeline/YYYY-MM-DD.json`, one file per day, kept (blocks, model calls, tokens). `version`
//! 2 added `meta`; a version 1 file reads as before, its details empty until a pass on that day fills them.
//! Island: `day` on the left dock after Settled (`preset: note`, title "Today", a `timeline` part the islands app
//! draws as a day ribbon and one row per block). The user's place, and whether it is open, are his.

use crate::islands::files::Root;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const PERIOD: Duration = Duration::from_secs(15 * 60);
/// The first pass after a start.
const FIRST: Duration = Duration::from_secs(60);
pub const ISLAND: &str = "day";
pub const TITLE: &str = "Today";
pub const ICON: &str = "calendar.day.timeline.left";
/// The day runs from 05:00 local to 05:00 the next morning.
pub const DAY_START_HOUR: u32 = 5;
pub const SEGMENT_S: i64 = 15 * 60;
/// A segment counts as active with this much screen time or any speech.
const ACTIVE_SCREEN_S: i64 = 60;
/// How far before the day a screen state that runs into it may have begun.
const SCREEN_LOOKBACK_S: i64 = 6 * 3600;
/// Blocks that ended this long ago are fixed.
const FREEZE_AFTER_S: i64 = 60 * 60;
/// What one segment carries to the model.
const LINES_PER_SEGMENT: usize = 14;
const LINE_CHARS: usize = 220;
const TITLES_PER_SEGMENT: usize = 6;
pub const BLOCK_TITLE_CHARS: usize = 32;
pub const BLOCK_LINE_CHARS: usize = 60;
pub const KINDS: [&str; 6] = ["build", "talk", "people", "web", "play", "other"];
/// The standard `activity:` tag: what kind of time a block was, finer than its colour.
pub const ACTIVITIES: [&str; 13] = ["agent work", "build", "planning", "call", "chat", "browsing", "watching", "reading", "gaming", "music", "break", "admin", "other"];
/// The day file's layout: 2 added `Block::meta`.
pub const VERSION: u32 = 2;
const ABOUT_CHARS: usize = 400;
const META_ITEM_CHARS: usize = 120;
const META_ITEMS: usize = 8;
const EFFORT: &str = "medium";
/// The log and ledger label.
pub const JOB: &str = "day timeline";

// ---------------------------------------------------------------------------------------------------------
// The archive, read-only.

#[derive(Clone, Debug, PartialEq)]
pub struct Screen {
    pub start: i64,
    pub end: i64,
    pub app: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub at: i64,
    pub speaker: String,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Evidence {
    pub screens: Vec<Screen>,
    pub lines: Vec<Line>,
}

/// Screen states overlapping `[from, to)` and final transcript lines that start in it (epoch seconds), from the
/// archive index.
pub fn read_archive(archive: &Path, from: i64, to: i64) -> Result<Evidence> {
    let index = archive.join(heimdall_archive::store::INDEX);
    ensure!(index.exists(), "no archive index at {}", index.display());
    let db = rusqlite::Connection::open_with_flags(&index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let (a, b) = (from * 1_000_000_000, to * 1_000_000_000);
    let tag = |key: &str| format!("(SELECT value FROM chunk_tags WHERE chunk_id=c.id AND key='{key}' ORDER BY value LIMIT 1)");
    let mut ev = Evidence::default();
    // Screen states that overlap the span, also one that began before it (`segments` clips them); the lower bound
    // keeps the time index in use.
    let sql = format!("SELECT c.start_wall_ns,c.end_wall_ns,{},{} FROM timeline_chunks c WHERE c.kind='screen' AND c.start_wall_ns>=?3 AND c.start_wall_ns<?2 AND c.end_wall_ns>?1 ORDER BY c.start_wall_ns", tag("app"), tag("window_title"));
    let mut q = db.prepare(&sql)?;
    let rows = q.query_map([a, b, a - SCREEN_LOOKBACK_S * 1_000_000_000], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?)))?;
    for row in rows {
        let (s, e, app, title) = row?;
        let Some(app) = app.filter(|a| !a.is_empty()) else { continue };
        ev.screens.push(Screen { start: s / 1_000_000_000, end: (e / 1_000_000_000).max(s / 1_000_000_000), app, title: title.filter(|t| !t.trim().is_empty()) });
    }
    let sql = format!("SELECT c.start_wall_ns,{},c.text FROM timeline_chunks c WHERE c.kind='text' AND c.role='utterance' AND c.start_wall_ns>=?1 AND c.start_wall_ns<?2 ORDER BY c.start_wall_ns", tag("speaker"));
    let mut q = db.prepare(&sql)?;
    let rows = q.query_map([a, b], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (s, speaker, text) = row?;
        let text = one_line(&text);
        if text.is_empty() { continue }
        ev.lines.push(Line { at: s / 1_000_000_000, speaker: speaker.unwrap_or_else(|| "unknown speaker".into()), text });
    }
    Ok(ev)
}

// ---------------------------------------------------------------------------------------------------------
// Days, apps and segments: no model.

/// The day `at` belongs to (`DAY_START_HOUR` local to the next one) and its start, epoch seconds.
pub fn day_of(at: i64) -> (String, i64) {
    use chrono::{Duration as D, Local, TimeZone};
    let t = Local.timestamp_opt(at, 0).single().unwrap_or_else(Local::now);
    // On the wall clock, so the hour a clock change repeats or skips stays on its side of 05:00.
    let date = (t.naive_local() - D::hours(DAY_START_HOUR as i64)).date();
    let start = date.and_hms_opt(DAY_START_HOUR, 0, 0).and_then(|n| Local.from_local_datetime(&n).earliest()).map_or(at, |d| d.timestamp());
    (date.format("%Y-%m-%d").to_string(), start)
}

/// An app's name as the user knows it, from its bundle id.
pub fn app_name(bundle: &str) -> String {
    const NAMES: [(&str, &str); 24] = [
        ("com.citrolabs.ego", "Ego"), ("com.hnc.Discord", "Discord"), ("com.t3tools.t3code", "T3 Code"),
        ("dev.heimdall.app", "Heimdall"), ("dev.arsenal.app", "Arsenal"), ("com.apple.finder", "Finder"),
        ("com.apple.Preview", "Preview"), ("com.brave.Browser", "Brave"), ("com.valvesoftware.steam", "Steam"),
        ("com.apple.Terminal", "Terminal"), ("com.mitchellh.ghostty", "Ghostty"), ("com.apple.dt.Xcode", "Xcode"),
        ("com.apple.Safari", "Safari"), ("com.google.Chrome", "Chrome"), ("com.apple.Music", "Music"),
        ("com.spotify.client", "Spotify"), ("ch.protonmail", "Proton Mail"), ("com.apple.Console", "Console"),
        ("com.apple.TextEdit", "TextEdit"), ("com.microsoft.VSCode", "VS Code"), ("dev.vault.floatingislands", "Islands"),
        ("dev.vella", "Vella"), ("com.apple.systempreferences", "Settings"), ("dev.filmcraft", "FilmCraft"),
    ];
    if let Some((_, n)) = NAMES.iter().find(|(b, _)| bundle == *b || bundle.starts_with(&format!("{b}."))) { return (*n).into() }
    let last = bundle.rsplit('.').next().unwrap_or(bundle);
    let mut c = last.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// What an app's time usually is (the deterministic guess; the model may say otherwise).
pub fn app_kind(bundle: &str) -> &'static str {
    let b = bundle.to_ascii_lowercase();
    let any = |xs: &[&str]| xs.iter().any(|x| b.contains(x));
    if any(&["t3code", "terminal", "ghostty", "iterm", "xcode", "vscode", "cursor", "zed", "dev.heimdall", "dev.arsenal", "floatingislands", "console", "filmcraft", "claude"]) { "build" }
    else if any(&["discord", "messages", "telegram", "whatsapp", "slack", "protonmail", "facetime", "zoom", "mail"]) { "people" }
    else if any(&["steam", "music", "spotify", "iina", "vlc", "tv", "game"]) { "play" }
    else if any(&["ego", "browser", "safari", "chrome", "firefox", "arc", "preview", "orion"]) { "web" }
    else { "other" }
}

/// Speakers that are the user, Heimdall or nobody in particular.
fn is_guest(speaker: &str) -> bool {
    !matches!(speaker, "User" | "Heimdall") && !speaker.to_ascii_lowercase().starts_with("unknown")
}

/// A 15-minute slice of the day as the tags show it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Segment {
    /// Its place on the day's grid (ids stay unique through a clock change).
    pub index: usize,
    pub start: i64,
    /// The grid end (the next segment's start), or now for the current one.
    pub end: i64,
    /// First and last evidence inside it.
    pub first: i64,
    pub last: i64,
    /// (app name, seconds), most first.
    pub apps: Vec<(String, i64)>,
    pub titles: Vec<String>,
    /// T3 Code's window titles: the agent threads in front.
    pub threads: Vec<String>,
    /// (speaker, lines), most first.
    pub people: Vec<(String, usize)>,
    pub lines: Vec<Line>,
    pub kind: &'static str,
}

impl Segment {
    pub fn id(&self) -> String { format!("s{:02}", self.index) }
    pub fn active(&self) -> bool { self.apps.iter().map(|a| a.1).sum::<i64>() >= ACTIVE_SCREEN_S || !self.lines.is_empty() }
    fn digest(&self) -> u64 {
        let mut h = DefaultHasher::new();
        (self.start, self.apps.iter().map(|a| (&a.0, a.1 / 60)).collect::<Vec<_>>(), &self.titles, self.lines.len()).hash(&mut h);
        h.finish()
    }
}

/// The day's segments from `day_start` up to `now`, on the fixed grid (inactive ones included).
pub fn segments(ev: &Evidence, day_start: i64, now: i64) -> Vec<Segment> {
    let n = ((now - day_start).max(0) + SEGMENT_S - 1) / SEGMENT_S;
    let mut segs: Vec<Segment> = (0..n).map(|k| {
        let start = day_start + k * SEGMENT_S;
        Segment { index: k as usize, start, end: (start + SEGMENT_S).min(now), first: i64::MAX, last: i64::MIN, kind: "other", ..Default::default() }
    }).collect();
    let mut apps: Vec<BTreeMap<String, (i64, &'static str)>> = vec![BTreeMap::new(); segs.len()];
    let mut titles: Vec<BTreeMap<String, i64>> = vec![BTreeMap::new(); segs.len()];
    let mut threads: Vec<BTreeMap<String, i64>> = vec![BTreeMap::new(); segs.len()];
    for s in &ev.screens {
        let (a, b) = (s.start.max(day_start), s.end.min(now));
        if b < a { continue }
        let mut k = ((a - day_start) / SEGMENT_S) as usize;
        while k < segs.len() && segs[k].start < b.max(a + 1) {
            let seg = &mut segs[k];
            let overlap = b.min(seg.start + SEGMENT_S) - a.max(seg.start);
            let e = apps[k].entry(app_name(&s.app)).or_insert((0, app_kind(&s.app)));
            e.0 += overlap.max(0);
            if let Some(t) = &s.title {
                *titles[k].entry(t.clone()).or_default() += overlap.max(1);
                let name = app_name(&s.app);
                if name == "T3 Code" && t.trim() != name { *threads[k].entry(one_line(t)).or_default() += overlap.max(1); }
            }
            seg.first = seg.first.min(a.max(seg.start));
            seg.last = seg.last.max(b.min(seg.start + SEGMENT_S));
            k += 1;
        }
    }
    for l in &ev.lines {
        if l.at < day_start || l.at >= now { continue }
        let seg = &mut segs[((l.at - day_start) / SEGMENT_S) as usize];
        seg.first = seg.first.min(l.at);
        seg.last = seg.last.max(l.at);
        seg.lines.push(l.clone());
    }
    for (k, seg) in segs.iter_mut().enumerate() {
        let mut a: Vec<(String, i64, &'static str)> = apps[k].iter().map(|(n, (s, kind))| (n.clone(), *s, *kind)).collect();
        a.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        let mut t: Vec<(String, i64)> = titles[k].iter().map(|(n, s)| (n.clone(), *s)).collect();
        t.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        seg.titles = t.into_iter().take(TITLES_PER_SEGMENT).map(|t| clip(&t.0, 80)).collect();
        let mut th: Vec<(String, i64)> = threads[k].iter().map(|(n, s)| (n.clone(), *s)).collect();
        th.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        seg.threads = th.into_iter().take(TITLES_PER_SEGMENT).map(|t| clip(&t.0, 80)).collect();
        let mut people: HashMap<String, usize> = HashMap::new();
        for l in &seg.lines { *people.entry(l.speaker.clone()).or_default() += 1; }
        let mut p: Vec<(String, usize)> = people.into_iter().collect();
        p.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        seg.people = p;
        let guests: usize = seg.people.iter().filter(|p| is_guest(&p.0)).map(|p| p.1).sum();
        let heimdall = seg.people.iter().filter(|p| p.0 == "Heimdall").map(|p| p.1).sum::<usize>();
        let screen: i64 = a.iter().map(|x| x.1).sum();
        // The kind with the most screen time; people heard on a call win over the app in front.
        let mut by_kind: BTreeMap<&'static str, i64> = BTreeMap::new();
        for x in &a { *by_kind.entry(x.2).or_default() += x.1; }
        let top = by_kind.iter().max_by_key(|(k, s)| (**s, std::cmp::Reverse(**k))).map(|(k, _)| *k);
        seg.kind = if guests >= 3 { "people" } else if heimdall >= 3 && top != Some("build") { "talk" } else { top.unwrap_or(if screen == 0 && !seg.lines.is_empty() { "talk" } else { "other" }) };
        seg.apps = a.into_iter().map(|x| (x.0, x.1)).collect();
        if seg.first == i64::MAX { seg.first = seg.start; seg.last = seg.start; }
    }
    segs
}

// ---------------------------------------------------------------------------------------------------------
// Blocks and the day file.

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Epoch seconds: the first and last evidence in it.
    pub start: i64,
    pub end: i64,
    pub kind: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub line: String,
    /// The segments it covers (`Segment::id`).
    pub segments: Vec<String>,
    /// Named by the model (else by its apps).
    #[serde(default)]
    pub named: bool,
    /// What the row does not show (absent in a version 1 file).
    #[serde(default, skip_serializing_if = "Meta::is_empty")]
    pub meta: Meta,
}

/// A block's details, kept for the open row and for search. Lists are unique, in the order first seen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// The project worked on ("Heimdall"), when there was one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// One of `ACTIVITIES`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub activity: String,
    /// A few sentences on what happened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub about: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<String>,
    /// Agent threads (T3 windows, threads named in the conversation).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<String>,
    /// Tasks set, done or handed to an agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    /// (app, minutes), most first; from the tags only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<(String, i64)>,
}

impl Meta {
    pub fn is_empty(&self) -> bool { *self == Meta::default() }

    /// The standard tags: `project:<name>`, `activity:<one of ACTIVITIES>`, `app:<name>` (lowercase, so a search
    /// for `project:heimdall` finds "Heimdall").
    pub fn tags(&self) -> Vec<String> {
        let mut t = vec![];
        if !self.project.is_empty() { t.push(format!("project:{}", self.project.to_lowercase())) }
        if !self.activity.is_empty() { t.push(format!("activity:{}", self.activity)) }
        for a in &self.apps { t.push(format!("app:{}", a.0.to_lowercase())) }
        t
    }

    /// `other`'s lists added to these (each once); its project, activity and explanation where these have none.
    fn absorb(&mut self, other: &Meta) {
        for (a, b) in [(&mut self.people, &other.people), (&mut self.threads, &other.threads), (&mut self.tasks, &other.tasks), (&mut self.files, &other.files), (&mut self.decisions, &other.decisions)] {
            for x in b { add_unique(a, x) }
        }
        for (a, b) in [(&mut self.project, &other.project), (&mut self.activity, &other.activity), (&mut self.about, &other.about)] {
            if a.is_empty() { a.clone_from(b) }
        }
    }

    /// What the tags say about `segs`, replacing the apps and adding to the lists (so it can run on every pass).
    fn add_facts(&mut self, segs: &[&Segment], kind: &str) {
        let mut apps: BTreeMap<&str, i64> = BTreeMap::new();
        for s in segs {
            for a in &s.apps { *apps.entry(&a.0).or_default() += a.1 }
            for p in s.people.iter().filter(|p| is_guest(&p.0)) { add_unique(&mut self.people, &p.0) }
            for t in &s.threads { add_unique(&mut self.threads, t) }
            for w in &s.titles { for f in file_names(w) { add_unique(&mut self.files, &f) } }
        }
        let mut apps: Vec<(String, i64)> = apps.into_iter().filter(|a| a.1 >= 60).map(|(n, s)| (n.to_owned(), (s + 30) / 60)).collect();
        apps.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.apps = apps;
        if self.activity.is_empty() { self.activity = activity_of(kind, &self.apps, !self.people.is_empty()).into() }
        for l in [&mut self.people, &mut self.threads, &mut self.files] { l.truncate(META_ITEMS * 2) }
    }
}

fn add_unique(list: &mut Vec<String>, x: &str) {
    let x = x.trim();
    if !x.is_empty() && !list.iter().any(|y| y.eq_ignore_ascii_case(x)) { list.push(x.to_owned()) }
}

/// The activity a block's colour and apps suggest, without a model.
pub fn activity_of(kind: &str, apps: &[(String, i64)], guests: bool) -> &'static str {
    let top = apps.first().map(|a| a.0.as_str()).unwrap_or("");
    match kind {
        "build" if matches!(top, "Xcode" | "VS Code" | "Cursor" | "Zed") => "build",
        "build" => "agent work",
        "talk" => "planning",
        "people" if guests => "call",
        "people" => "chat",
        "web" if top == "Preview" => "reading",
        "web" => "browsing",
        "play" if matches!(top, "Music" | "Spotify") => "music",
        "play" if top == "Steam" => "gaming",
        "play" => "watching",
        _ => "other",
    }
}

/// File names in a window title ("day_timeline.rs — Heimdall" → `day_timeline.rs`): a word with a known extension.
pub fn file_names(title: &str) -> Vec<String> {
    const EXT: [&str; 26] = ["rs", "swift", "md", "py", "json", "toml", "ts", "tsx", "js", "jsx", "html", "css", "sh", "txt", "yaml", "yml", "png", "jpg", "pdf", "mov", "mp4", "wav", "csv", "sql", "c", "h"];
    title.split(|c: char| c.is_whitespace() || matches!(c, '—' | '–' | '|' | '"' | '\'' | '(' | ')' | ',' | ':'))
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter_map(|w| {
            let name = w.rsplit('/').next()?;
            let (stem, ext) = name.rsplit_once('.')?;
            (!stem.is_empty() && EXT.contains(&ext.to_ascii_lowercase().as_str())).then(|| name.to_owned())
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Day {
    /// The file's layout (`VERSION`; missing: 1).
    #[serde(default = "v1")]
    pub version: u32,
    pub date: String,
    pub start: i64,
    pub updated: i64,
    pub blocks: Vec<Block>,
    /// Blocks before this index never change.
    #[serde(default)]
    pub frozen: usize,
    /// What the open tail looked like at the last naming (unchanged: no call).
    #[serde(default)]
    pub tail_digest: u64,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub calls: u64,
    /// The whole prompt: uncached input, cache reads and cache writes.
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub failed: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// At list price (`background_call::MODELS`).
    #[serde(default)]
    pub dollars: f64,
}

pub fn day_path(data: &Path, date: &str) -> PathBuf { data.join("day-timeline").join(format!("{date}.json")) }

fn v1() -> u32 { 1 }

/// A day file, brought up to `VERSION` in memory (the file is rewritten only by the next pass on it).
pub fn load_day(data: &Path, date: &str) -> Option<Day> {
    parse_day(&std::fs::read(day_path(data, date)).ok()?)
}

pub fn parse_day(bytes: &[u8]) -> Option<Day> {
    let mut day: Day = serde_json::from_slice(bytes).ok()?;
    migrate(&mut day);
    Some(day)
}

/// Version 1 to 2: the blocks read as they were, with empty details; today's fill on its next pass, from the
/// archive (an older day keeps its kind, which search still finds as `kind:`).
pub fn migrate(day: &mut Day) {
    day.version = VERSION;
}

/// Blocks across the day files in `data` (newest day first) whose words or tags hold every word of `query`, any
/// case: `project:heimdall`, `activity:call`, `app:discord` or plain words ("Morgan islands").
pub fn search(data: &Path, query: &str) -> Vec<(String, Block)> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut dates: Vec<String> = std::fs::read_dir(data.join("day-timeline")).map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_owned)).collect()).unwrap_or_default();
    dates.sort_by(|a, b| b.cmp(a));
    let mut out = vec![];
    for date in dates {
        let Some(day) = load_day(data, &date) else { continue };
        for b in day.blocks {
            let hay = b.search_text();
            if !words.is_empty() && words.iter().all(|w| hay.contains(w.as_str())) { out.push((date.clone(), b)) }
        }
    }
    out
}

impl Block {
    /// Everything a search looks through, lowercase: the row, the details and the tags.
    pub fn search_text(&self) -> String {
        let m = &self.meta;
        let mut t = vec![self.title.clone(), self.line.clone(), format!("kind:{}", self.kind), m.about.clone()];
        t.extend(m.tags());
        for l in [&m.people, &m.threads, &m.tasks, &m.files, &m.decisions] { t.extend(l.iter().cloned()) }
        t.join("\n").to_lowercase()
    }
}

pub fn save_day(data: &Path, day: &Day) -> Result<()> {
    let p = day_path(data, &day.date);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(day)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

/// Runs of active segments, split where more than 30 minutes of inactive segments sit between them.
fn runs(segs: &[&Segment]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![];
    for (i, s) in segs.iter().enumerate() {
        let joined = i > 0 && (s.start - segs[i - 1].start) <= 3 * SEGMENT_S;
        if joined { out.last_mut().unwrap().push(i) } else { out.push(vec![i]) }
    }
    out
}

fn block_of(segs: &[&Segment], kind: &str, title: String, line: String, named: bool) -> Block {
    Block {
        start: segs.iter().map(|s| s.first).min().unwrap_or(0),
        end: segs.iter().map(|s| s.last).max().unwrap_or(0),
        kind: kind.into(),
        title,
        line,
        segments: segs.iter().map(|s| s.id()).collect(),
        named,
        meta: { let mut m = Meta::default(); m.add_facts(segs, kind); m },
    }
}

/// Blocks without a model: neighbouring active segments of one kind (and, on a call, the same people) make a block,
/// named by its main apps or the people on the call.
pub fn fallback_blocks(active: &[&Segment]) -> Vec<Block> {
    let mut out = vec![];
    for run in runs(active) {
        let mut start = 0;
        while start < run.len() {
            let kind = active[run[start]].kind;
            let mut end = start + 1;
            while end < run.len() && active[run[end]].kind == kind { end += 1 }
            let segs: Vec<&Segment> = run[start..end].iter().map(|&i| active[i]).collect();
            let mut apps: BTreeMap<&str, i64> = BTreeMap::new();
            let mut guests: BTreeMap<&str, usize> = BTreeMap::new();
            for s in &segs {
                for a in &s.apps { *apps.entry(&a.0).or_default() += a.1 }
                for p in s.people.iter().filter(|p| is_guest(&p.0)) { *guests.entry(&p.0).or_default() += p.1 }
            }
            let mut apps: Vec<(&str, i64)> = apps.into_iter().collect();
            apps.sort_by(|a, b| b.1.cmp(&a.1));
            let mut guests: Vec<(&str, usize)> = guests.into_iter().collect();
            guests.sort_by(|a, b| b.1.cmp(&a.1));
            let title = if kind == "people" && !guests.is_empty() {
                format!("Call with {}", guests.iter().take(2).map(|g| g.0).collect::<Vec<_>>().join(", "))
            } else if kind == "talk" && apps.is_empty() {
                "With Heimdall".into()
            } else {
                apps.iter().take(2).map(|a| a.0).collect::<Vec<_>>().join(" and ")
            };
            out.push(block_of(&segs, kind, clip(&title, BLOCK_TITLE_CHARS), String::new(), false));
            start = end;
        }
    }
    out
}

/// Which segments are still open: those after the last frozen block.
fn open_segments<'a>(day: &Day, active: &[&'a Segment]) -> Vec<&'a Segment> {
    let fixed_end = day.blocks[..day.frozen.min(day.blocks.len())].iter().map(|b| b.end).max().unwrap_or(i64::MIN);
    let fixed: std::collections::HashSet<&String> = day.blocks[..day.frozen.min(day.blocks.len())].iter().flat_map(|b| b.segments.iter()).collect();
    active.iter().copied().filter(|s| !fixed.contains(&s.id()) && s.last > fixed_end).collect()
}

fn tail_digest(open: &[&Segment]) -> u64 {
    let mut h = DefaultHasher::new();
    for s in open { s.digest().hash(&mut h) }
    h.finish()
}

/// Freezes every block but the last that ended `FREEZE_AFTER_S` before `now`, once the model named it (a morning
/// named by app while there was no model gets its names when the model is back).
fn freeze(day: &mut Day, now: i64) {
    while day.frozen + 1 < day.blocks.len() && day.blocks[day.frozen].named && day.blocks[day.frozen].end <= now - FREEZE_AFTER_S { day.frozen += 1 }
}

// ---------------------------------------------------------------------------------------------------------
// The model: group and name the open segments.

pub const SYSTEM: &str = "You keep User's \"Where the day went\" timeline: a glanceable picture of his day, shown on a small island on his Mac. \
Heimdall is his voice assistant; most of his work is talking with Heimdall, which runs coding agents for him, so the conversation says what the work was about. \
You get the newest 15-minute segments of today, each with the apps in front (minutes), window titles, who was heard and lines of what was said. \
Group consecutive segments into blocks of one project or activity, and name each block.\n\
Rules:\n\
- Cover every segment exactly once, in order; a block is consecutive segments. Never join across a gap of more than 30 minutes.\n\
- Prefer fewer, longer blocks: split only where the project or activity clearly changes (a call starts, another project). A block is usually 30 minutes to 2 hours. A block of a single segment only when that quarter is clearly its own thing; a short aside joins the block around it.\n\
- title: 1 to 4 words, the project first when there is one (\"Heimdall islands\", \"Arsenal site\", \"Call with Morgan\", \"YouTube\", \"Dota\"). No dates, no times, no \"User\".\n\
- line: one short past-tense line on what got done or decided, concrete (\"Menu branch merged; ack voices picked\"). At most 40 characters: it shows on one short line; drop words rather than run long. Empty when nothing can be said.\n\
- kind: build (projects, coding, agents at work), talk (planning or talking things through with Heimdall), people (calls and chats with others), web (reading, browsing, watching), play (games, music, fun), other.\n\
The rest is kept for when User opens the block; never guess, leave a field empty when the segments do not say:\n\
- project: the project's name as User says it (\"Heimdall\", \"Arsenal\"), else empty. activity: the closest of the listed activities (break: away or resting).\n\
- about: two to four plain sentences on what happened and why, concrete, past tense.\n\
- people: other people talked with or about (not User, not Heimdall). threads: agent threads or chats named or in front. tasks: tasks set, done or handed to an agent. files: files or documents named. decisions: what User decided, one short line each.\n\
- Speech from other people's apps and videos can show up as unknown speakers; do not treat it as User's work.\n\
- Write in English. Return only the JSON.";

pub fn schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["blocks"],
        "properties": {"blocks": {"type": "array", "items": {
            "type": "object", "additionalProperties": false, "required": ["segments", "title", "line", "kind", "project", "activity", "about", "people", "threads", "tasks", "files", "decisions"],
            "properties": {
                "segments": {"type": "array", "items": {"type": "string"}},
                "title": {"type": "string"},
                "line": {"type": "string"},
                "kind": {"type": "string", "enum": KINDS},
                "project": {"type": "string"},
                "activity": {"type": "string", "enum": ACTIVITIES},
                "about": {"type": "string"},
                "people": {"type": "array", "items": {"type": "string"}},
                "threads": {"type": "array", "items": {"type": "string"}},
                "tasks": {"type": "array", "items": {"type": "string"}},
                "files": {"type": "array", "items": {"type": "string"}},
                "decisions": {"type": "array", "items": {"type": "string"}}
            }
        }}}
    })
}

/// The run on the background model (`background_call`: adaptive thinking, structured output, the one-hour cache
/// on the system prompt and the fixed blocks, the $0.50 limit).
pub fn call(model: &str, prompt: crate::background_call::Prompt) -> crate::background_call::Call<'static> {
    crate::background_call::Call { job: JOB, model: model.to_owned(), system: SYSTEM, schema: schema(), effort: EFFORT, prompt, history: Vec::new(), session: None }
}

#[cfg(test)]
pub fn request_body(model: &str, prompt: crate::background_call::Prompt) -> Value {
    crate::background_call::prepare(&call(model, prompt)).map(|p| p.body).unwrap_or(Value::Null)
}

fn segment_text(s: &Segment) -> String {
    let mut t = format!("<segment id=\"{}\" time=\"{}–{}\" guess=\"{}\">\n", s.id(), local_hhmm(s.start), local_hhmm(s.start + SEGMENT_S), s.kind);
    if !s.apps.is_empty() {
        t += &format!("apps: {}\n", s.apps.iter().filter(|a| a.1 >= 20).map(|a| format!("{} {} min", a.0, (a.1 + 30) / 60)).collect::<Vec<_>>().join(", "));
    }
    if !s.titles.is_empty() { t += &format!("windows: {}\n", s.titles.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ")) }
    if !s.people.is_empty() { t += &format!("heard: {}\n", s.people.iter().map(|p| format!("{} ({})", p.0, p.1)).collect::<Vec<_>>().join(", ")) }
    // The user's and Heimdall's lines first (the work), then a few of everyone else's, kept in time order.
    let own: Vec<usize> = (0..s.lines.len()).filter(|&i| matches!(s.lines[i].speaker.as_str(), "User" | "Heimdall")).collect();
    let others: Vec<usize> = (0..s.lines.len()).filter(|&i| !own.contains(&i)).collect();
    let mut pick: Vec<usize> = spread(&own, LINES_PER_SEGMENT - LINES_PER_SEGMENT.min(others.len()).min(4));
    pick.extend(spread(&others, LINES_PER_SEGMENT - pick.len()));
    pick.sort();
    for i in pick {
        let l = &s.lines[i];
        t += &format!("{} {}: {}\n", local_hhmm(l.at), l.speaker, clip(&l.text, LINE_CHARS));
    }
    t + "</segment>\n"
}

/// At most `n` of `xs`, evenly spread over them.
fn spread(xs: &[usize], n: usize) -> Vec<usize> {
    if xs.len() <= n { return xs.to_vec() }
    (0..n).map(|k| xs[k * xs.len() / n]).collect()
}

/// Stable first, for the prompt cache: the fixed blocks (they change only when a block freezes), then the last
/// answer, the open segments, and last the clock and the instruction.
pub fn user_prompt(day: &Day, open: &[&Segment], now: i64) -> crate::background_call::Prompt {
    let mut prompt = crate::background_call::Prompt::default();
    let fixed = &day.blocks[..day.frozen.min(day.blocks.len())];
    if !fixed.is_empty() {
        let mut p = String::from("<earlier_today note=\"fixed; for context only\">\n");
        for b in fixed { p += &format!("{}–{} {} ({}): {}\n", local_hhmm(b.start), local_hhmm(b.end), b.title, b.kind, b.line) }
        p += "</earlier_today>\n\n";
        prompt.stable(p);
    }
    let mut p = String::new();
    let before: Vec<&Block> = day.blocks[day.frozen.min(day.blocks.len())..].iter().filter(|b| b.named).collect();
    if !before.is_empty() {
        p += "<your_last_answer note=\"keep these names unless the new segments change what they were\">\n";
        for b in before { p += &format!("{}: {} ({}): {}\n", b.segments.join(","), b.title, b.kind, b.line) }
        p += "</your_last_answer>\n\n";
    }
    p += "<segments>\n";
    for s in open { p += &segment_text(s) }
    p += "</segments>\n\n";
    p += &format!("Now: {}\n\nGroup and name these segments now.", local_hhmm(now));
    prompt.push(p);
    prompt
}

/// The answer, checked: every open segment once, in order (ids that are not open segments are passed over), blocks of consecutive segments without a long gap, names
/// capped. Anything else is an error (the caller keeps the segments' own blocks).
pub fn validate(answer: &str, open: &[&Segment]) -> Result<Vec<Block>> {
    let v: Value = serde_json::from_str(answer.trim()).context("not JSON")?;
    let blocks = v["blocks"].as_array().context("no blocks")?;
    let index: HashMap<String, usize> = open.iter().enumerate().map(|(i, s)| (s.id(), i)).collect();
    let mut next = 0usize;
    let mut out = vec![];
    for b in blocks {
        let ids: Vec<&str> = b["segments"].as_array().context("segments")?.iter().filter_map(Value::as_str).collect();
        ensure!(!ids.is_empty(), "a block without segments");
        let mut segs: Vec<&Segment> = vec![];
        // An id that is not an open segment (the model filling a quiet quarter) is passed over.
        for id in ids {
            let Some(&i) = index.get(id) else { continue };
            ensure!(i == next, "segment {id} out of order");
            if let Some(prev) = segs.last() { ensure!(open[i].start - prev.start <= 3 * SEGMENT_S, "a block across a gap at {id}") }
            segs.push(open[i]);
            next += 1;
        }
        if segs.is_empty() { continue }
        let title = clip(&one_line(b["title"].as_str().unwrap_or("")), BLOCK_TITLE_CHARS);
        ensure!(!title.is_empty(), "a block without a title");
        let kind = b["kind"].as_str().filter(|k| KINDS.contains(k)).unwrap_or("other");
        let mut block = block_of(&segs, kind, title, clip(&one_line(b["line"].as_str().unwrap_or("")), BLOCK_LINE_CHARS), true);
        let mut said = Meta {
            project: clip(&one_line(b["project"].as_str().unwrap_or("")), BLOCK_TITLE_CHARS),
            activity: b["activity"].as_str().filter(|a| ACTIVITIES.contains(a)).unwrap_or("").into(),
            about: clip(&one_line(b["about"].as_str().unwrap_or("")), ABOUT_CHARS),
            ..Default::default()
        };
        for x in b["people"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|p| is_guest(p)).take(META_ITEMS) { add_unique(&mut said.people, &clip(&one_line(x), META_ITEM_CHARS)) }
        for (key, list) in [ ("threads", &mut said.threads), ("tasks", &mut said.tasks), ("files", &mut said.files), ("decisions", &mut said.decisions)] {
            for x in b[key].as_array().into_iter().flatten().filter_map(Value::as_str).take(META_ITEMS) { add_unique(list, &clip(&one_line(x), META_ITEM_CHARS)) }
        }
        // The model's word first (its activity over the guess from the apps), then what the tags add.
        said.absorb(&block.meta);
        said.apps = std::mem::take(&mut block.meta.apps);
        block.meta = said;
        out.push(block);
    }
    ensure!(next == open.len(), "{} of {} segments covered", next, open.len());
    Ok(merge_same(out))
}

/// Neighbouring blocks with the same title and kind are one block (the later line says where it ended up).
fn merge_same(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = vec![];
    for b in blocks {
        match out.last_mut() {
            Some(a) if a.title == b.title && a.kind == b.kind && b.start - a.end <= 2 * SEGMENT_S => {
                a.end = a.end.max(b.end);
                a.segments.extend(b.segments);
                if !b.line.is_empty() { a.line = b.line; }
                let about = std::mem::take(&mut a.meta.about);
                a.meta.absorb(&b.meta);
                a.meta.about = clip(&[about, b.meta.about].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "), ABOUT_CHARS);
                let mut apps: BTreeMap<String, i64> = BTreeMap::new();
                for (n, m) in a.meta.apps.drain(..).chain(b.meta.apps) { *apps.entry(n).or_default() += m }
                a.meta.apps = apps.into_iter().collect();
                a.meta.apps.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
            }
            _ => out.push(b),
        }
    }
    out
}

pub use crate::background_call::Answer;

/// How a pass reaches the model (`None`: no model this pass).
pub type Ask = Arc<dyn Fn(&crate::background_call::Call) -> Result<Answer> + Send + Sync>;

// ---------------------------------------------------------------------------------------------------------
// One pass.

/// What a pass did (logs, the replay).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pass {
    pub called: bool,
    pub failed: Option<String>,
    pub blocks: usize,
    pub open_segments: usize,
}

/// Rebuild today's timeline as of `now` from `ev` (today's evidence up to now), naming the open tail with `ask` when
/// it changed. Returns the day as it is now and what happened.
pub fn update(prev: Option<Day>, ev: &Evidence, now: i64, ask: Option<&Ask>, model: &str) -> (Day, Pass) {
    let (date, start) = day_of(now);
    let mut day = prev.filter(|d| d.date == date).unwrap_or(Day { date, start, ..Default::default() });
    migrate(&mut day);
    let segs = segments(ev, start, now);
    let active: Vec<&Segment> = segs.iter().filter(|s| s.active()).collect();
    day.frozen = day.frozen.min(day.blocks.len());
    let open = open_segments(&day, &active);
    let digest = tail_digest(&open);
    let mut pass = Pass { open_segments: open.len(), ..Default::default() };
    // Whether the open tail stands as the model named it (`None`: untouched this pass).
    let mut named_now: Option<bool> = None;
    let unchanged = digest == day.tail_digest && day.blocks.len() > day.frozen;
    if !open.is_empty() && !unchanged {
        let fallback = || fallback_blocks(&open);
        let mut named = false;
        // Paid for without an answer, paid for an answer `validate` rejects, or over the limit: asked again only when
        // the open segments change (the same input would give the same bad answer).
        let mut settled = false;
        let mut answered = false;
        let tail = match ask {
            Some(ask) => {
                pass.called = true;
                day.usage.calls += 1;
                match ask(&call(model, user_prompt(&day, &open, now))).and_then(|a| {
                    day.usage.input_tokens += a.usage.prompt();
                    day.usage.output_tokens += a.usage.output;
                    day.usage.cache_read_tokens += a.usage.cache_read;
                    day.usage.cache_write_tokens += a.usage.cache_write();
                    day.usage.dollars += a.usd;
                    answered = true;
                    validate(&a.text, &open)
                }) {
                    Ok(b) => { named = true; b }
                    // Agent off or emergency mode's pause: nothing went out; the app's names stand in, no failure.
                    Err(e) if crate::background_gate::quiet(&e) => { pass.called = false; day.usage.calls -= 1; fallback() }
                    Err(e) => {
                        if let Some(s) = e.downcast_ref::<crate::background_call::Stopped>() {
                            day.usage.input_tokens += s.usage.prompt();
                            day.usage.output_tokens += s.usage.output;
                            day.usage.cache_read_tokens += s.usage.cache_read;
                            day.usage.cache_write_tokens += s.usage.cache_write();
                            day.usage.dollars += s.usd;
                        }
                        settled = answered || crate::background_call::settled(&e);
                        day.usage.failed += 1;
                        pass.failed = Some(format!("{e:#}"));
                        fallback()
                    }
                }
            }
            None => fallback(),
        };
        // A failed or absent naming keeps the model's last names where the same segments are still one block.
        let old: Vec<Block> = day.blocks.drain(day.frozen..).collect();
        let tail = tail.into_iter().map(|b| {
            if b.named { return b }
            old.iter().find(|o| o.named && o.segments.first() == b.segments.first()).map(|o| {
                let mut meta = o.meta.clone();
                meta.absorb(&b.meta);
                meta.apps = b.meta.apps.clone();
                Block { title: o.title.clone(), line: o.line.clone(), kind: o.kind.clone(), named: true, meta, ..b.clone() }
            }).unwrap_or(b)
        });
        day.blocks.extend(tail);
        named_now = Some(named || settled);
    } else if !open.is_empty() {
        // Unchanged segments; the current block may still end later (its last evidence moved within a segment).
        for b in day.blocks[day.frozen..].iter_mut() {
            let segs: Vec<&&Segment> = open.iter().filter(|s| b.segments.contains(&s.id())).collect();
            if let (Some(a), Some(z)) = (segs.iter().map(|s| s.first).min(), segs.iter().map(|s| s.last).max()) { b.start = a; b.end = z; }
        }
    }
    // The tags' facts for the open blocks; a fixed block gets them once, when it has none (a version 1 day), and
    // never changes after.
    let by_id: HashMap<String, &Segment> = segs.iter().map(|s| (s.id(), s)).collect();
    let frozen = day.frozen.min(day.blocks.len());
    for (k, b) in day.blocks.iter_mut().enumerate() {
        if k < frozen && !b.meta.activity.is_empty() { continue }
        let mine: Vec<&Segment> = b.segments.iter().filter_map(|id| by_id.get(id).copied()).collect();
        if !mine.is_empty() { b.meta.add_facts(&mine, &b.kind) }
    }
    freeze(&mut day, now);
    // Named by the model: the next pass calls again only for something new in what is still open (after this
    // freeze). Unnamed: asked again next pass.
    match named_now {
        Some(true) => day.tail_digest = tail_digest(&open_segments(&day, &active)),
        Some(false) => day.tail_digest = 0,
        // Unchanged and named: still named once a block froze.
        None if day.tail_digest == digest && digest != 0 => day.tail_digest = tail_digest(&open_segments(&day, &active)),
        None => {}
    }
    day.updated = now;
    pass.blocks = day.blocks.len();
    (day, pass)
}

/// The island for `day`: title, icon, the left dock after Settled (only when it is new: the user's place and open state
/// are his), and the timeline part.
pub fn island(day: &Day, now: i64, existing: Option<&Map<String, Value>>) -> Map<String, Value> {
    let blocks: Vec<Value> = day.blocks.iter().map(|b| {
        let mut o = json!({"start": b.start, "end": b.end, "kind": b.kind, "title": b.title, "line": b.line});
        if let Some(m) = island_meta(b) { o["meta"] = m }
        o
    }).collect();
    let mut o = json!({
        "preset": "note", "title": TITLE, "icon": ICON, "day": day.date,
        "parts": [{"type": "timeline", "from": day.start, "now": now, "blocks": blocks}],
        "updated": now, "hidden": null,
    }).as_object().unwrap().clone();
    if existing.is_none() {
        for (k, v) in [("side", json!("left")), ("order", json!(3)), ("expanded", json!(false)), ("started", json!(now))] { o.insert(k.into(), v); }
    }
    o
}

/// What an open row shows that the row does not already say: the explanation, the project unless the title names
/// it, people the title does not name, threads, tasks, files and decisions (`None`: nothing to open). The activity
/// stays in the file and in search; on the island the colour says it.
fn island_meta(b: &Block) -> Option<Value> {
    let (m, title) = (&b.meta, b.title.to_lowercase());
    let named = |x: &String| title.contains(&x.to_lowercase());
    let tags: Vec<&String> = [&m.project].into_iter().filter(|p| !p.is_empty() && !named(p)).collect();
    let people: Vec<&String> = m.people.iter().filter(|p| !named(p)).collect();
    let mut o = Map::new();
    if !m.about.is_empty() { o.insert("about".into(), json!(m.about)); }
    for (k, v) in [("tags", json!(tags)), ("people", json!(people)), ("threads", json!(m.threads)), ("tasks", json!(m.tasks)), ("files", json!(m.files)), ("decisions", json!(m.decisions))] {
        if v.as_array().is_some_and(|a| !a.is_empty()) { o.insert(k.into(), v); }
    }
    (!o.is_empty()).then_some(Value::Object(o))
}

/// Write the island. Before anything happened today, yesterday's island is hidden, not removed: its place and open
/// state stay the user's, and it shows again with today's first block.
pub fn show(root: &Root, day: &Day, now: i64) -> Result<()> {
    let existing = root.read(ISLAND);
    if day.blocks.is_empty() {
        if existing.as_ref().is_some_and(|o| o.get("day").and_then(Value::as_str) != Some(day.date.as_str())) {
            let mut o = island(day, now, existing.as_ref());
            o.insert("hidden".into(), json!(true));
            root.put(ISLAND, &o)?;
        }
        return Ok(());
    }
    root.put(ISLAND, &island(day, now, existing.as_ref()))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// The background loop.

pub struct Config {
    /// Heimdall's data folder: `archive/` and `day-timeline/`.
    pub data: PathBuf,
    pub islands: Root,
    /// Background Tasks ▸ Day Timeline.
    pub enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The model this pass (`None`: name by app), and its id.
    pub model: Arc<dyn Fn() -> Option<(Ask, String)> + Send + Sync>,
}

/// One pass at `now`: read, update, store, show.
pub fn run_pass(cfg: &Config, now: i64) -> Result<Pass> {
    let (date, start) = day_of(now);
    let ev = read_archive(&cfg.data.join("archive"), start, now + 1)?;
    let model = (cfg.model)();
    let (day, pass) = update(load_day(&cfg.data, &date), &ev, now, model.as_ref().map(|m| &m.0), model.as_ref().map_or("", |m| m.1.as_str()));
    save_day(&cfg.data, &day)?;
    show(&cfg.islands, &day, now)?;
    if let Some(e) = &pass.failed { eprintln!("Day timeline: naming failed, blocks named by app this pass: {e}"); }
    Ok(pass)
}

pub fn spawn(cfg: Config, mut stop: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    let cfg = Arc::new(cfg);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + FIRST, PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                // Emergency mode ended: the pass it held back runs now, then the usual period.
                _ = crate::background_gate::resumed() => { tick.reset(); }
                r = stop.changed() => { if r.is_err() || *stop.borrow() { break } else { continue } }
            }
            if *stop.borrow() { break }
            // Emergency mode's pause: no pass at all (no request, no "naming failed").
            if !(cfg.enabled)() || crate::background_gate::emergency_paused().is_some() { continue }
            let c = cfg.clone();
            match tokio::task::spawn_blocking(move || run_pass(&c, chrono::Utc::now().timestamp())).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("Day timeline: pass failed: {e:#}"),
                Err(e) => eprintln!("Day timeline: pass panicked: {e}"),
            }
        }
    })
}

/// The installed loop: Heimdall's archive, the islands host's folder, the background model while the agent is on.
/// No islands host: nothing runs.
pub fn spawn_live(data: &Path, gate: Arc<agent::delivery::DeliveryGate>, stop: tokio::sync::watch::Receiver<bool>) -> Option<tokio::task::JoinHandle<()>> {
    if cfg!(test) { return None }
    let host = crate::islands::host()?;
    let cfg = Config {
        data: data.into(),
        islands: host.root.clone(),
        enabled: Arc::new(|| crate::background_model::enabled(crate::background_model::Task::DayTimeline)),
        // The conversation leaves the Mac only for the chosen Anthropic model, and only while the agent is on.
        model: Arc::new(move || {
            gate.check().ok()?;
            // Its own model when the user gave it one (Background Tasks ▸ Day Timeline), else the shared one.
            let e = crate::background_model::task_endpoint(crate::background_model::Task::DayTimeline);
            e.key.as_ref()?;
            let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
            Some((Arc::new(move |c: &crate::background_call::Call| crate::background_call::ask_blocking(&e, c)) as Ask, model))
        }),
    };
    Some(spawn(cfg, stop))
}

// ---------------------------------------------------------------------------------------------------------

fn local_hhmm(at: i64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(at, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default()
}

fn one_line(t: &str) -> String { t.split_whitespace().collect::<Vec<_>>().join(" ") }

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { return s.to_owned() }
    s.chars().take(n.saturating_sub(1)).collect::<String>().trim_end().to_owned() + "…"
}

#[cfg(test)]
#[path = "day_timeline_tests.rs"]
mod tests;
