//! The `timeline` part (Heimdall's Day Timeline, "where the day went"): blocks of the day with a kind, a title and a
//! line, drawn as a day ribbon (each block its colour, gaps empty, a tick at now) over one row per block whose rail
//! grows with its length. This is the geometry, without AppKit.
//! `{"type": "timeline", "from": epoch s, "now": epoch s, "blocks": [{"start", "end", "kind", "title", "line", "meta"}]}`
//! Kinds: build, talk, people, web, play, other (anything else draws as other). `meta` is what an open row shows:
//! `{"about": "…", "tags": [..], "people": [..], "threads": [..], "tasks": [..], "files": [..], "decisions": [..]}`,
//! all optional.
//! No blocks yet: the empty ribbon from the day's start to now.

use super::{j, Json};

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub start: f64,
    pub end: f64,
    pub kind: String,
    pub title: String,
    pub line: Option<String>,
    pub meta: Meta,
}

/// What a block's row shows once it is opened.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub about: Option<String>,
    pub tags: Vec<String>,
    pub people: Vec<String>,
    pub threads: Vec<String>,
    pub tasks: Vec<String>,
    pub files: Vec<String>,
    pub decisions: Vec<String>,
}

impl Meta {
    fn parse(m: Option<&Json>) -> Meta {
        let Some(m) = m else { return Meta::default() };
        let list = |k: &str| -> Vec<String> {
            j::arr(m.get(k)).map(|a| a.iter().filter_map(|v| j::nonempty(Some(v))).map(str::to_owned).take(12).collect()).unwrap_or_default()
        };
        Meta { about: j::nonempty(m.get("about")).map(str::to_owned), tags: list("tags"), people: list("people"), threads: list("threads"), tasks: list("tasks"), files: list("files"), decisions: list("decisions") }
    }

    pub fn is_empty(&self) -> bool { *self == Meta::default() }

    /// The lists an open row shows, each with its SF Symbol, in order.
    pub fn lists(&self) -> Vec<(&'static str, &[String])> {
        [("person.2", &self.people[..]), ("bubble.left.and.text.bubble.right", &self.threads[..]), ("checklist", &self.tasks[..]), ("doc", &self.files[..]), ("checkmark.seal", &self.decisions[..])]
            .into_iter().filter(|l| !l.1.is_empty()).collect()
    }
}

impl Block {
    /// The row's key for opening it (its start: blocks never share one).
    pub fn key(&self) -> String { format!("tl:{}", self.start as i64) }
    pub fn discloses(&self) -> bool { !self.meta.is_empty() }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timeline {
    /// The ribbon's span: whole hours around the blocks and now.
    pub from: f64,
    pub to: f64,
    pub now: f64,
    pub blocks: Vec<Block>,
}

pub const KINDS: [&str; 6] = ["build", "talk", "people", "web", "play", "other"];
/// What an empty day shows under its ribbon (the Today island's own symbol).
pub const EMPTY_SYMBOL: &str = "calendar.day.timeline.left";
/// At most this many rows (the newest); the ribbon still shows the whole day.
pub const MAX_ROWS: usize = 24;

impl Timeline {
    pub fn parse(p: &Json) -> Option<Timeline> {
        let now = j::num(p.get("now"))?;
        let mut blocks: Vec<Block> = j::objs(p.get("blocks"))?
            .into_iter()
            .filter_map(|b| {
                let (start, end) = (j::num(b.get("start"))?, j::num(b.get("end"))?);
                let title = j::nonempty(b.get("title"))?.to_owned();
                let kind = j::str(b.get("kind")).filter(|k| KINDS.contains(k)).unwrap_or("other").to_owned();
                let meta = Meta::parse(j::obj(b.get("meta")));
                (end >= start).then_some(Block { start, end: end.min(now.max(start)), kind, title, line: j::nonempty(b.get("line")).map(str::to_owned), meta })
            })
            .collect();
        blocks.sort_by(|a, b| a.start.total_cmp(&b.start));
        // No blocks yet: from the day's start (at most three hours back) to now.
        let first = blocks.first().map(|b| b.start).unwrap_or_else(|| j::num(p.get("from")).unwrap_or(now).max(now - 3.0 * 3600.0));
        let last = blocks.iter().map(|b| b.end).fold(now, f64::max);
        let from = (first / 3600.0).floor() * 3600.0;
        let to = ((last / 3600.0).ceil() * 3600.0).max(from + 3.0 * 3600.0);
        Some(Timeline { from, to, now, blocks })
    }

    /// Where `t` sits on a ribbon `width` wide.
    pub fn x(&self, t: f64, width: f64) -> f64 {
        ((t - self.from) / (self.to - self.from)).clamp(0.0, 1.0) * width
    }

    /// Each block's span on the ribbon (at least `min` wide, so a short block still shows).
    pub fn spans(&self, width: f64, min: f64) -> Vec<(f64, f64)> {
        self.blocks.iter().map(|b| {
            let (a, z) = (self.x(b.start, width), self.x(b.end, width));
            let w = (z - a).max(min);
            (a.min(width - w).max(0.0), w)
        }).collect()
    }

    /// The hours labelled under the ribbon: every 3 hours on a long day, every 2 or 1 on a short one, as epoch
    /// seconds on whole hours.
    pub fn ticks(&self) -> Vec<f64> {
        let hours = ((self.to - self.from) / 3600.0).round() as i64;
        let step = if hours > 12 { 3 } else if hours > 6 { 2 } else { 1 };
        (0..=hours).step_by(step as usize).map(|h| self.from + h as f64 * 3600.0).collect()
    }

    /// The rows shown, oldest first: the newest `MAX_ROWS`.
    pub fn rows(&self) -> &[Block] {
        &self.blocks[self.blocks.len().saturating_sub(MAX_ROWS)..]
    }

    /// The kinds on the ribbon, in `KINDS` order: what the legend names.
    pub fn kinds(&self) -> Vec<&'static str> {
        KINDS.iter().copied().filter(|k| self.blocks.iter().any(|b| b.kind == *k)).collect()
    }

    /// The block still going (it reaches within 20 minutes of now).
    pub fn current(&self) -> Option<usize> {
        let i = self.blocks.len().checked_sub(1)?;
        (self.now - self.blocks[i].end <= 20.0 * 60.0).then_some(i)
    }
}

/// How much taller a block's rail is than its text: nothing up to half an hour, then a point per five minutes, at
/// most 24 points (a long block reads longer at a glance).
/// The legend's name for a kind's colour.
pub fn kind_name(kind: &str) -> &'static str {
    match kind {
        "build" => "Build",
        "talk" => "Heimdall",
        "people" => "People",
        "web" => "Web",
        "play" => "Play",
        _ => "Other",
    }
}

pub fn rail_extra(minutes: f64) -> f64 {
    ((minutes - 30.0) / 5.0).clamp(0.0, 24.0)
}

/// "45 min", "1 h 20" (a block's length, compact).
pub fn length(secs: f64) -> String {
    let m = (secs / 60.0).round().max(1.0) as i64;
    if m < 60 {
        format!("{m} min")
    } else if m % 60 == 0 {
        format!("{} h", m / 60)
    } else {
        format!("{} h {}", m / 60, m % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn part(v: serde_json::Value) -> Json { v.as_object().unwrap().clone() }

    #[test]
    fn parses_blocks_in_order_and_spans_whole_hours() {
        let t = Timeline::parse(&part(json!({"type": "timeline", "now": 36000 + 5400, "blocks": [
            {"start": 36000 + 3600, "end": 36000 + 5000, "kind": "people", "title": "Call"},
            {"start": 36000 + 600, "end": 36000 + 2400, "kind": "weird", "title": "Heimdall", "line": "Merged"},
            {"start": 1, "end": 2, "kind": "build", "title": ""}]}))).unwrap();
        assert_eq!(t.blocks.len(), 2, "a block without a title is left out");
        assert_eq!((t.blocks[0].title.as_str(), t.blocks[0].kind.as_str()), ("Heimdall", "other"));
        assert_eq!((t.from, t.to), (36000.0, 36000.0 + 3.0 * 3600.0), "at least three hours");
        assert_eq!(t.current(), Some(1));
        let s = t.spans(300.0, 3.0);
        assert!((s[0].0 - 600.0 / 10800.0 * 300.0).abs() < 1e-9 && s[1].0 > s[0].0 + s[0].1);
        assert_eq!(t.ticks(), vec![36000.0, 39600.0, 43200.0, 46800.0]);
        assert!(Timeline::parse(&part(json!({"now": 1}))).is_none(), "no blocks list: no timeline");
        let empty = Timeline::parse(&part(json!({"from": 36000, "now": 36000 + 5400, "blocks": []}))).unwrap();
        assert_eq!((empty.from, empty.to, empty.current(), empty.kinds().len()), (36000.0, 36000.0 + 3.0 * 3600.0, None, 0), "an empty day is the bare ribbon");
        assert_eq!(t.kinds(), vec!["people", "other"]);
        assert_eq!(kind_name("talk"), "Heimdall");
    }

    #[test]
    fn a_long_day_labels_every_third_hour_and_short_blocks_stay_visible() {
        let t = Timeline::parse(&part(json!({"now": 15.0 * 3600.0, "blocks": [
            {"start": 0, "end": 60, "kind": "build", "title": "A"},
            {"start": 14.0 * 3600.0, "end": 15.0 * 3600.0, "kind": "web", "title": "B"}]}))).unwrap();
        assert_eq!(t.ticks().len(), 6);
        assert!(t.spans(300.0, 3.0)[0].1 >= 3.0);
        assert_eq!(length(45.0 * 60.0), "45 min");
        assert_eq!(length(80.0 * 60.0), "1 h 20");
        assert_eq!(length(120.0 * 60.0), "2 h");
        assert_eq!(rail_extra(20.0), 0.0);
        assert_eq!(rail_extra(500.0), 24.0);
    }

    #[test]
    fn a_blocks_details_are_read_and_open_only_when_there_are_some() {
        let t = Timeline::parse(&part(json!({"now": 7200, "blocks": [
            {"start": 0, "end": 600, "kind": "build", "title": "A", "meta": {"about": "Merged.", "tags": ["Heimdall", ""], "people": ["Morgan"], "files": [], "decisions": ["Keep it"]}},
            {"start": 900, "end": 1200, "kind": "web", "title": "B"}]}))).unwrap();
        let m = &t.blocks[0].meta;
        assert_eq!((m.about.as_deref(), m.tags.clone(), m.people.clone()), (Some("Merged."), vec!["Heimdall".to_string()], vec!["Morgan".to_string()]));
        assert_eq!(m.lists().iter().map(|l| l.0).collect::<Vec<_>>(), vec!["person.2", "checkmark.seal"]);
        assert!(t.blocks[0].discloses() && !t.blocks[1].discloses());
        assert_eq!(t.blocks[1].key(), "tl:900");
    }
}
