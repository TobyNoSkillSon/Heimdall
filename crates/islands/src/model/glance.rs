//! The glance (on the "Jarvis voice cleanup" island): a job island's status leaves the top for
//! one compact row at the bottom, right of the open chip, so the content gets the room; and wherever a value allows
//! it, a small live graphic shows it instead of words: a bar for a fraction or a percentage, a sparkline (the
//! decider's training-loss line, small) for a number that changes. Platform-neutral: what to draw, not how.
//!
//! `Series` keeps the numbers an island showed over its rewrites (in memory, the newest last), so a value the agent
//! rewrites (`kv` "Memory 41 GB", `progress`) grows its own line.

use std::collections::{BTreeMap, BTreeSet};

use super::island::Island;

/// Points kept per series: enough for a slope, few enough to stay a glance.
pub const SERIES_CAP: usize = 24;

/// A value read from an island's text.
#[derive(Clone, Debug, PartialEq)]
pub enum Quantity {
    /// "7/10", "7 / 10", "7 of 10 images".
    Fraction { done: f64, total: f64 },
    /// "42%", "42 %".
    Percent(f64),
    /// "41 GB", "31 tok/s", "0.57": a number first, then at most a short unit without digits.
    Number(f64),
}

fn number_prefix(s: &str) -> Option<(f64, &str)> {
    let s = s.trim_start();
    let end = s
        .char_indices()
        .take_while(|(k, c)| c.is_ascii_digit() || *c == '.' || *c == ',' || (*k == 0 && (*c == '-' || *c == '−')))
        .map(|(k, c)| k + c.len_utf8())
        .last()?;
    let digits: String = s[..end].chars().filter(|c| *c != ',').map(|c| if c == '−' { '-' } else { c }).collect();
    let v: f64 = digits.parse().ok().filter(|v: &f64| v.is_finite())?;
    // "1,5" in a Polish text is not one thousand five; a comma only groups thousands ("12,400").
    if s[..end].contains(',') && !s[..end].split(',').skip(1).all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    Some((v, &s[end..]))
}

/// What a value says as a quantity, when it is one.
pub fn quantity(s: &str) -> Option<Quantity> {
    let (v, rest) = number_prefix(s)?;
    let rest = rest.trim();
    if rest == "%" {
        return Some(Quantity::Percent(v));
    }
    let after = rest.strip_prefix('/').or_else(|| rest.strip_prefix("of ")).map(str::trim_start);
    if let Some(after) = after {
        let (total, unit) = number_prefix(after)?;
        let unit = unit.trim();
        if total > 0.0 && v >= 0.0 && unit.chars().count() <= 14 && !unit.chars().any(|c| c.is_ascii_digit()) {
            return Some(Quantity::Fraction { done: v, total });
        }
        return None;
    }
    // A number and a short unit ("41 GB", "31 tok/s", "3.2 s"); anything longer is a sentence, not a value.
    if rest.chars().count() <= 8 && !rest.chars().any(|c| c.is_ascii_digit()) {
        return Some(Quantity::Number(v));
    }
    None
}

/// The graphic a value earns.
#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    None,
    /// A short capsule filled to 0…1.
    Bar(f64),
    /// A line through these points, the last one marked (at least two, not all the same).
    Spark(Vec<f64>),
}

/// The graphic for one key value: a bar for a fraction or a percentage, a sparkline once a number has changed.
pub fn kv_mark(value: &str, history: &[f64]) -> Mark {
    match quantity(value) {
        Some(Quantity::Fraction { done, total }) => Mark::Bar((done / total).clamp(0.0, 1.0)),
        Some(Quantity::Percent(p)) => Mark::Bar((p / 100.0).clamp(0.0, 1.0)),
        Some(Quantity::Number(_)) => spark(history),
        None => Mark::None,
    }
}

fn spark(points: &[f64]) -> Mark {
    let moved = points.windows(2).any(|w| (w[0] - w[1]).abs() > f64::EPSILON);
    if points.len() >= 2 && moved {
        Mark::Spark(points.to_vec())
    } else {
        Mark::None
    }
}

/// The numbers an island showed, by series key (`progress`, `kv:<label>`: a number, or a share as 0…1), the newest
/// last; and which of them moved in the latest version.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Series {
    values: BTreeMap<String, Vec<f64>>,
    moved: BTreeSet<String>,
    /// Per key, whether it holds shares (0…1) rather than plain numbers: a change of kind starts the line over.
    shares: BTreeMap<String, bool>,
}

impl Series {
    pub fn get(&self, key: &str) -> &[f64] {
        self.values.get(key).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// The value before the latest, when the latest version moved it: what a rewrite's arrival starts from.
    pub fn before(&self, key: &str) -> Option<f64> {
        let v = self.get(key);
        (self.moved.contains(key) && v.len() >= 2).then(|| v[v.len() - 2])
    }

    fn push(&mut self, key: String, v: f64) {
        let s = self.values.entry(key.clone()).or_default();
        // A rewrite that leaves the value alone adds nothing: the line moves only when the number does.
        if s.last().is_some_and(|l| (l - v).abs() <= f64::EPSILON) {
            return;
        }
        s.push(v);
        if s.len() > SERIES_CAP {
            s.remove(0);
        }
        self.moved.insert(key);
    }

    /// Takes in one version of the island: its progress and every key value that is a quantity. A key value that
    /// is no longer one, or gone, starts over.
    pub fn record(&mut self, i: &Island) {
        self.moved.clear();
        match i.progress() {
            Some(p) => self.push("progress".into(), p),
            None => {
                self.values.remove("progress");
            }
        }
        let facts = i.facts();
        self.values.retain(|k, _| !k.starts_with("kv:") || facts.iter().any(|(l, _)| k[3..] == **l));
        for (label, value) in facts {
            let key = format!("kv:{label}");
            let q = quantity(&value);
            let share = matches!(q, Some(Quantity::Fraction { .. } | Quantity::Percent(_)));
            if q.is_some() && self.shares.insert(key.clone(), share).is_some_and(|was| was != share) {
                self.values.remove(&key);
            }
            match q {
                Some(Quantity::Number(v)) => self.push(key, v),
                Some(Quantity::Fraction { done, total }) => self.push(key, (done / total).clamp(0.0, 1.0)),
                Some(Quantity::Percent(p)) => self.push(key, (p / 100.0).clamp(0.0, 1.0)),
                None => {
                    self.values.remove(&key);
                }
            }
        }
    }
}

/// How the glance's words are coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    /// Waiting on the user: orange, as the island's dot.
    Waiting,
    /// A result's value: full white.
    Value,
}

/// The duration the glance ends with, beside a timer glyph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Clock {
    None,
    /// Running or waiting since this moment (epoch s): grows once a minute while the island is open.
    Since(f64),
    /// Finished or failed: how long it took, in seconds.
    Took(f64),
}

/// The bottom row's right side: a graphic and its figure, a few words, and a duration.
#[derive(Clone, Debug, PartialEq)]
pub struct Glance {
    pub mark: Mark,
    pub figure: Option<String>,
    pub words: Option<String>,
    pub tone: Tone,
    pub clock: Clock,
}

impl Glance {
    pub fn is_empty(&self) -> bool {
        self.mark == Mark::None && self.figure.is_none() && self.words.is_none() && self.clock == Clock::None
    }
}

/// Stages that only repeat what the island's icon already says.
const SAID_BY_ICON: [&str; 4] = ["Done", "Failed", "Finished", "Complete"];

/// The glance for a job island (`look` task, waiting, result or failed). `header_ring`: the header's icon is the
/// progress ring already, so the glance does not repeat it. `born`: when the file appeared, for writers that leave
/// out `started`.
pub fn glance(i: &Island, series: &Series, header_ring: bool, born: Option<f64>) -> Glance {
    let look = i.look();
    let started = i.started().or(born);
    let stage = i.stage().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty() && !SAID_BY_ICON.iter().any(|w| w.eq_ignore_ascii_case(s)));
    let mut g = Glance { mark: Mark::None, figure: None, words: stage, tone: Tone::Plain, clock: Clock::None };
    match look {
        "task" => {
            if let Some(p) = i.progress() {
                // The header's ring draws the share already; the figure stays (the open header shows no number).
                if !header_ring {
                    g.mark = match spark(series.get("progress")) {
                        Mark::Spark(pts) => Mark::Spark(pts),
                        _ => Mark::Bar(p),
                    };
                }
                g.figure = Some(format!("{}%", (p * 100.0).round() as i64));
            }
            g.clock = started.map(Clock::Since).unwrap_or(Clock::None);
        }
        "waiting" => {
            g.tone = Tone::Waiting;
            g.clock = started.map(Clock::Since).unwrap_or(Clock::None);
        }
        "result" | "failed" => {
            if look == "result" {
                if let Some(v) = i.value() {
                    g.words = Some(v);
                    g.tone = Tone::Value;
                }
            }
            let ended = i.ended().or(i.updated());
            g.clock = match (started, ended) {
                (Some(s), Some(e)) if e - s >= 1.0 => Clock::Took(e - s),
                _ => Clock::None,
            };
        }
        _ => {}
    }
    g
}

/// A duration as the glance writes it: "<1 min", "7 min", "1 h 5", "3 d".
pub fn short_duration(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0) as i64;
    if m < 1 {
        return "<1 min".into();
    }
    if m < 60 {
        return format!("{m} min");
    }
    if m < 24 * 60 {
        return if m % 60 == 0 { format!("{} h", m / 60) } else { format!("{} h {}", m / 60, m % 60) };
    }
    format!("{} d", m / (24 * 60))
}

/// The clock's words at `now`.
pub fn clock_text(c: Clock, now: f64) -> Option<String> {
    match c {
        Clock::None => None,
        Clock::Since(t) => Some(short_duration(now - t)),
        Clock::Took(d) => Some(short_duration(d.max(60.0))),
    }
}

/// Where a sparkline's points sit in a `w` × `h` box (y down, `inset` kept clear at every edge so the round caps and
/// the end dot are not clipped): evenly spaced, lowest value at the bottom. A flat line sits in the middle.
pub fn spark_points(values: &[f64], w: f64, h: f64, inset: f64) -> Vec<(f64, f64)> {
    if values.len() < 2 {
        return vec![];
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = hi - lo;
    let n = (values.len() - 1) as f64;
    values
        .iter()
        .enumerate()
        .map(|(k, v)| {
            let x = inset + (w - 2.0 * inset) * k as f64 / n;
            let f = if span > 1e-12 { (v - lo) / span } else { 0.5 };
            (x, inset + (h - 2.0 * inset) * (1.0 - f))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Json;
    use serde_json::json;

    fn island(v: serde_json::Value) -> Island {
        let o: Json = v.as_object().unwrap().clone();
        Island::new("t", o)
    }

    #[test]
    fn values_read_as_fractions_percentages_or_numbers_and_sentences_stay_text() {
        assert_eq!(quantity("7/10"), Some(Quantity::Fraction { done: 7.0, total: 10.0 }));
        assert_eq!(quantity("7 / 10 images"), Some(Quantity::Fraction { done: 7.0, total: 10.0 }));
        assert_eq!(quantity("3 of 12 tests"), Some(Quantity::Fraction { done: 3.0, total: 12.0 }));
        assert_eq!(quantity("42%"), Some(Quantity::Percent(42.0)));
        assert_eq!(quantity("41 GB"), Some(Quantity::Number(41.0)));
        assert_eq!(quantity("31 tok/s"), Some(Quantity::Number(31.0)));
        assert_eq!(quantity("12,400 steps"), Some(Quantity::Number(12400.0)));
        assert_eq!(quantity("-0.5"), Some(Quantity::Number(-0.5)));
        for text in ["NLL 0.5733 at step 900", "Not run yet", "0.57 at step 900", "1,5 GB", "3/0", "2026-10-26", ""] {
            assert_eq!(quantity(text), None, "{text}");
        }
    }

    #[test]
    fn a_value_earns_a_bar_for_a_share_and_a_sparkline_once_it_moved() {
        assert_eq!(kv_mark("7/10", &[]), Mark::Bar(0.7));
        assert_eq!(kv_mark("130%", &[]), Mark::Bar(1.0));
        assert_eq!(kv_mark("41 GB", &[41.0]), Mark::None, "one point is no line");
        assert_eq!(kv_mark("41 GB", &[41.0, 41.0]), Mark::None, "a flat history is no line");
        assert_eq!(kv_mark("43 GB", &[41.0, 43.0]), Mark::Spark(vec![41.0, 43.0]));
        assert_eq!(kv_mark("Not run yet", &[1.0, 2.0]), Mark::None);
    }

    #[test]
    fn series_follow_rewrites_only_when_the_number_moves_and_forget_gone_values() {
        let mut s = Series::default();
        s.record(&island(json!({"progress": 0.1, "kv": [{"label": "Memory", "value": "40 GB"}, {"label": "Gate", "value": "Not run"}]})));
        s.record(&island(json!({"progress": 0.1, "kv": [{"label": "Memory", "value": "40 GB"}]})));
        s.record(&island(json!({"progress": 0.3, "kv": [{"label": "Memory", "value": "44 GB"}]})));
        assert_eq!(s.get("progress"), &[0.1, 0.3]);
        assert_eq!(s.get("kv:Memory"), &[40.0, 44.0]);
        assert!(s.get("kv:Gate").is_empty());
        assert_eq!((s.before("progress"), s.before("kv:Memory")), (Some(0.1), Some(40.0)), "the latest version moved both");
        s.record(&island(json!({"progress": 0.3, "summary": "new words", "kv": [{"label": "Memory", "value": "44 GB"}]})));
        assert_eq!((s.before("progress"), s.before("kv:Memory")), (None, None), "a rewrite that moved nothing moves nothing");
        s.record(&island(json!({"kv": [{"label": "Memory", "value": "unknown"}]})));
        assert!(s.get("progress").is_empty() && s.get("kv:Memory").is_empty());
        for k in 0..40 {
            s.record(&island(json!({"progress": k as f64 / 40.0})));
        }
        assert_eq!(s.get("progress").len(), SERIES_CAP);
        assert_eq!(s.get("progress").last(), Some(&(39.0 / 40.0)));
        let mut s = Series::default();
        s.record(&island(json!({"kv": [{"label": "Images", "value": "3/10"}]})));
        s.record(&island(json!({"kv": [{"label": "Images", "value": "7/10"}]})));
        assert_eq!(s.before("kv:Images"), Some(0.3), "a share grows from where it was");
        s.record(&island(json!({"kv": [{"label": "Images", "value": "8 GB"}]})));
        assert_eq!(s.get("kv:Images"), &[8.0], "a share turned number starts over");
    }

    #[test]
    fn a_running_glance_is_stage_progress_and_time_and_never_repeats_the_ring() {
        let i = island(json!({"state": "running", "stage": "Building page", "progress": 0.42, "started": 1000.0}));
        let g = glance(&i, &Series::default(), false, None);
        assert_eq!((g.mark.clone(), g.figure.as_deref(), g.words.as_deref(), g.clock), (Mark::Bar(0.42), Some("42%"), Some("Building page"), Clock::Since(1000.0)));
        let g = glance(&i, &Series::default(), true, None);
        assert_eq!((g.mark, g.figure.as_deref()), (Mark::None, Some("42%")), "the header's ring draws it already; the number stays");
        let mut s = Series::default();
        s.record(&island(json!({"progress": 0.2})));
        s.record(&i);
        assert_eq!(glance(&i, &s, false, None).mark, Mark::Spark(vec![0.2, 0.42]), "a moving count shows its pace");
        assert_eq!(glance(&island(json!({"state": "running"})), &Series::default(), false, Some(5.0)).clock, Clock::Since(5.0), "file date when unstamped");
    }

    #[test]
    fn waiting_done_and_failed_glances() {
        let w = glance(&island(json!({"state": "waiting", "stage": "Your answer", "started": 10.0})), &Series::default(), false, None);
        assert_eq!((w.words.as_deref(), w.tone, w.clock), (Some("Your answer"), Tone::Waiting, Clock::Since(10.0)));
        let d = glance(&island(json!({"state": "done", "stage": "Done", "value": "3 / 3", "started": 0.0, "ended": 400.0})), &Series::default(), false, None);
        assert_eq!((d.words.as_deref(), d.tone, d.clock), (Some("3 / 3"), Tone::Value, Clock::Took(400.0)));
        let d = glance(&island(json!({"state": "done", "stage": "Done"})), &Series::default(), false, None);
        assert!(d.is_empty(), "the green check says done; nothing else to say: {d:?}");
        let f = glance(&island(json!({"state": "failed", "stage": "Build broke", "started": 0.0, "updated": 90.0})), &Series::default(), false, None);
        assert_eq!((f.words.as_deref(), f.clock), (Some("Build broke"), Clock::Took(90.0)));
    }

    #[test]
    fn durations_are_short() {
        assert_eq!(short_duration(20.0), "<1 min");
        assert_eq!(short_duration(7.0 * 60.0 + 5.0), "7 min");
        assert_eq!(short_duration(65.0 * 60.0), "1 h 5");
        assert_eq!(short_duration(120.0 * 60.0), "2 h");
        assert_eq!(short_duration(3.0 * 86400.0), "3 d");
        assert_eq!(clock_text(Clock::Took(10.0), 0.0).as_deref(), Some("1 min"), "a finished job took at least a minute");
        assert_eq!(clock_text(Clock::Since(0.0), 420.0).as_deref(), Some("7 min"));
    }

    #[test]
    fn sparkline_points_fill_the_box_inside_the_inset() {
        let p = spark_points(&[0.75, 0.66, 0.64, 0.58, 0.57], 40.0, 14.0, 2.0);
        assert_eq!(p.first(), Some(&(2.0, 2.0)), "the highest value at the top left");
        assert_eq!(p.last(), Some(&(38.0, 12.0)), "the lowest at the bottom right");
        assert!(p.windows(2).all(|w| w[1].0 > w[0].0));
        assert_eq!(spark_points(&[3.0, 3.0], 40.0, 14.0, 2.0)[1].1, 7.0, "flat sits in the middle");
        assert!(spark_points(&[1.0], 40.0, 14.0, 2.0).is_empty());
    }
}
