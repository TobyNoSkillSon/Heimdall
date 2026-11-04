//! Settled as proof of progress: the week's finished work and decisions, each a true dated item, bucketed
//! into the last seven local days. An item's day comes from its `at` (epoch seconds, written by the Goals job); a
//! legacy item's `when` counts only when it is an ISO date ("2026-10-31", that day at noon, or a full timestamp). A
//! word such as "Today" was written days ago and never updated, so such an item counts only in "before this week".

use super::due::Cal;
use super::plan::PlanItem;

/// Days in the window: six before today, then today.
pub const DAYS: usize = 7;

/// One finished item on its day.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub key: String,
    pub title: String,
    pub at: f64,
    pub decided: bool,
    pub discloses: bool,
}

/// The week as Settled draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct Week {
    /// Each day's local midnight, oldest first; the last is today.
    pub starts: [f64; DAYS],
    /// Each day's items in time order (oldest first).
    pub days: Vec<Vec<Mark>>,
    /// The local midnight of the first dated item of any age; `None` when nothing carries a trustworthy date.
    pub first_record: Option<f64>,
    /// Items older than the window, and legacy items whose `when` is a word.
    pub before: usize,
}

/// When an item happened: its `at`, else an ISO `when` (a bare date counts as that day's noon); `None` otherwise.
pub fn happened(it: &PlanItem, cal: &Cal) -> Option<f64> {
    if let Some(a) = it.at {
        return Some(a);
    }
    let w = it.when.as_deref()?.trim();
    if let Some(t) = super::time::parse_iso8601(w) {
        return Some(t);
    }
    // A bare date (its noon) or a local date and time without a zone ("2026-10-31T23:16", seconds optional).
    let b = w.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| w.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, se) = match b.len() {
        10 => (12, 0, 0),
        16 | 19 if matches!(b[10], b'T' | b't' | b' ') && b[13] == b':' => (n(11..13)?, n(14..16)?, if b.len() == 19 && b[16] == b':' { n(17..19)? } else { 0 }),
        _ => return None,
    };
    if h > 23 || mi > 59 || se > 59 {
        return None;
    }
    cal.local(y, m, d, h, mi, se)
}

impl Week {
    pub fn of(items: &[PlanItem], now: f64, cal: &Cal) -> Week {
        let today = cal.start_of_day(now);
        let mut starts = [today; DAYS];
        for (k, s) in starts.iter_mut().enumerate() {
            let back = (DAYS - 1 - k) as i64;
            *s = cal.add_days(-back, today).map(|t| cal.start_of_day(t + 3600.0)).unwrap_or(today - back as f64 * 86400.0);
        }
        let mut days: Vec<Vec<Mark>> = vec![vec![]; DAYS];
        let (mut first, mut before) = (None::<f64>, 0usize);
        for it in items {
            let Some(at) = happened(it, cal) else {
                before += 1;
                continue;
            };
            // A stamp ahead of the clock (skew, or a unit mistake) counts as now.
            let at = at.min(now);
            let day = cal.start_of_day(at);
            first = Some(first.map_or(day, |f: f64| f.min(day)));
            // Today is the last column; a stamp after today (a clock ahead) counts as today.
            let back = -cal.days(now, at).min(0);
            if back >= DAYS as i64 {
                before += 1;
                continue;
            }
            let mark = Mark { key: it.key.clone(), title: it.title.clone(), at, decided: it.decided(), discloses: it.detail.is_some() };
            days[DAYS - 1 - back as usize].push(mark);
        }
        for d in &mut days {
            d.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
        }
        Week { starts, days, first_record: first, before }
    }

    pub fn total(&self) -> usize {
        self.days.iter().map(Vec::len).sum()
    }

    pub fn decided(&self) -> usize {
        self.days.iter().flatten().filter(|m| m.decided).count()
    }

    pub fn shipped(&self) -> usize {
        self.total() - self.decided()
    }

    /// A day before the first record: drawn as "no record", not as a day without work.
    pub fn no_record(&self, d: usize) -> bool {
        self.first_record.is_none_or(|f| self.starts[d] < f)
    }

    /// The newest day that has items.
    pub fn newest(&self) -> Option<usize> {
        (0..DAYS).rev().find(|d| !self.days[*d].is_empty())
    }

    /// The running total at the end of each day.
    pub fn running(&self) -> [usize; DAYS] {
        let mut out = [0; DAYS];
        let mut n = 0;
        for (d, o) in out.iter_mut().enumerate() {
            n += self.days[d].len();
            *o = n;
        }
        out
    }

    pub fn most(&self) -> usize {
        self.days.iter().map(Vec::len).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Plan;

    fn week(items: serde_json::Value, now: f64, cal: &Cal) -> Week {
        let raw = crate::jobj! {"items": items};
        Week::of(&Plan::items(&raw), now, cal)
    }

    fn counts(w: &Week) -> Vec<usize> {
        w.days.iter().map(Vec::len).collect()
    }

    #[test]
    fn at_puts_an_item_on_its_local_day_across_midnight() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 11, 2, 3, 0, 0).unwrap();
        let a = |d: i64, h: i64, m: i64| cal.local(2026, 11, d, h, m, 0).unwrap();
        let w = week(
            serde_json::json!([
                {"id": "a", "title": "Just after midnight today", "at": a(2, 0, 1)},
                {"id": "b", "title": "Just before midnight", "at": a(1, 23, 59)},
                {"id": "c", "title": "Six days ago", "at": cal.local(2026, 10, 27, 9, 0, 0).unwrap(), "kind": "decided"},
                {"id": "d", "title": "Seven days ago", "at": cal.local(2026, 10, 26, 23, 0, 0).unwrap()},
                {"id": "e", "title": "Ahead of the clock", "at": a(3, 10, 0)},
            ]),
            now,
            &cal,
        );
        assert_eq!(counts(&w), [1, 0, 0, 0, 0, 1, 2]);
        assert_eq!(w.before, 1);
        assert!(w.days[6].iter().all(|m| m.at <= now), "a stamp ahead of the clock counts as now");
        assert_eq!((w.total(), w.shipped(), w.decided()), (4, 3, 1));
        assert_eq!(w.days[6][0].key, "a", "time order within the day");
        assert_eq!(w.starts[6], cal.local(2026, 11, 2, 0, 0, 0).unwrap());
        assert_eq!(w.starts[0], cal.local(2026, 10, 27, 0, 0, 0).unwrap());
        assert_eq!(w.running(), [1, 1, 1, 1, 1, 2, 4]);
        assert_eq!(w.newest(), Some(6));
        // The first record is the 26th (older than the window), so no day in the window is "no record".
        assert!((0..DAYS).all(|d| !w.no_record(d)));
    }

    #[test]
    fn the_window_survives_a_dst_change() {
        // Warsaw leaves summer time on 25 Oct 2026: that day is 25 hours long.
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 10, 28, 12, 0, 0).unwrap();
        let w = week(serde_json::json!([{"title": "Sunday night", "at": cal.local(2026, 10, 25, 23, 30, 0).unwrap()}]), now, &cal);
        assert_eq!(counts(&w), [0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(w.starts[3], cal.local(2026, 10, 25, 0, 0, 0).unwrap());
        assert_eq!(w.starts[4], cal.local(2026, 10, 26, 0, 0, 0).unwrap());
    }

    #[test]
    fn legacy_when_counts_an_iso_date_and_never_a_word() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 11, 2, 9, 0, 0).unwrap();
        let w = week(
            serde_json::json!([
                {"title": "ISO date", "when": "2026-10-31"},
                {"title": "Full stamp", "when": "2026-11-01T10:00:00Z"},
                {"title": "A word", "when": "Today"},
                {"title": "Another word", "when": "Yesterday"},
                {"title": "Nothing at all"},
                "A bare string",
                {"title": "Old ISO", "when": "2026-10-01"},
                {"title": "Bad date", "when": "2026-13-40"},
                {"title": "Local stamp", "when": "2026-10-31T23:16"},
            ]),
            now,
            &cal,
        );
        assert_eq!(counts(&w), [0, 0, 0, 0, 2, 1, 0]);
        assert_eq!(w.before, 6);
        assert_eq!(w.first_record, Some(cal.local(2026, 10, 1, 0, 0, 0).unwrap()));
        // `at` wins over `when`.
        let w = week(serde_json::json!([{"title": "Both", "when": "Today", "at": cal.local(2026, 10, 30, 8, 0, 0).unwrap()}]), now, &cal);
        assert_eq!(counts(&w), [0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn days_before_the_first_record_are_no_record_and_words_alone_make_none() {
        let cal = Cal::fixed("Europe/Warsaw", "en_GB");
        let now = cal.local(2026, 10, 28, 23, 30, 0).unwrap();
        let w = week(serde_json::json!([{"title": "First", "at": cal.local(2026, 10, 26, 17, 0, 0).unwrap()}]), now, &cal);
        assert_eq!((0..DAYS).filter(|d| w.no_record(*d)).count(), 4, "22-25 Oct");
        assert!(!w.no_record(4) && !w.no_record(6));
        let legacy = week(serde_json::json!([{"title": "a", "when": "Today"}, {"title": "b", "when": "Today"}]), now, &cal);
        assert_eq!((legacy.total(), legacy.before, legacy.first_record), (0, 2, None));
        assert!((0..DAYS).all(|d| legacy.no_record(d)));
        assert_eq!(legacy.newest(), None);
        // Only stamps ahead of the clock: they are today's, and today is on record.
        let ahead = week(serde_json::json!([{"title": "x", "at": now + 40.0 * 86400.0}]), now, &cal);
        assert_eq!((ahead.days[6].len(), ahead.no_record(6)), (1, false));
        let empty = week(serde_json::json!([]), now, &cal);
        assert_eq!((empty.total(), empty.before, empty.most()), (0, 0, 0));
    }

    #[test]
    fn at_kind_and_goal_parse_without_touching_other_lists() {
        let raw = crate::jobj! {"items": [
            {"id": "s-1", "title": "Arsenal: Klein fixes merged", "done": true, "at": 1793580000, "kind": "shipped", "goal": "g-arsenal", "detail": "", "by": "goals"},
            {"id": "s-2", "title": "Menu: Speaking becomes Personalisation", "at": "1793580000", "kind": "Decided", "goal": ""},
            {"id": "s-3", "title": "Milliseconds", "at": 1793580000000u64},
            {"id": "t", "title": "A waiting ask", "group": "waiting", "kind": "approve"},
        ]};
        let items = Plan::items(&raw);
        assert_eq!(items[0].at, Some(1793580000.0));
        assert_eq!(items[0].goal.as_deref(), Some("g-arsenal"));
        assert!(!items[0].decided() && items[1].decided());
        assert_eq!((items[1].at, items[1].goal.as_deref()), (Some(1793580000.0), None));
        assert_eq!(items[2].at, Some(1793580000.0));
        assert_eq!((items[3].at, items[3].live.kind.as_deref(), items[3].decided()), (None, Some("approve"), false));
        assert_eq!(items[0].detail, None, "an empty detail is no detail");
    }
}
