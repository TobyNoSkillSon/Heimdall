//! Scheduled items' due times ("proper integration"): writers store an absolute `due` (epoch
//! seconds); the app draws the countdown from the local clock, and a due item stays in Scheduled, drawn as due now.
//! The calendar maths goes through Foundation's NSCalendar, so days, midnights and time-zone changes match the Swift
//! build exactly (DST included).

use objc2::rc::Retained;
use objc2_foundation::{
    NSCalendar, NSCalendarIdentifierGregorian, NSCalendarOptions, NSCalendarUnit, NSDate, NSDateComponents, NSDateFormatter,
    NSLocale, NSString, NSTimeZone,
};
use serde_json::Value;

use super::plan::Plan;
use super::{time, Json};

/// A calendar: the user's own (`Cal::current`) or a fixed time zone (tests).
pub struct Cal {
    cal: Retained<NSCalendar>,
    locale: Option<Retained<NSLocale>>,
}

impl Cal {
    /// The user's calendar, time zone and locale as they are now (follows later changes).
    pub fn current() -> Cal {
        Cal { cal: NSCalendar::autoupdatingCurrentCalendar(), locale: None }
    }

    /// Gregorian in a named time zone with a locale identifier ("Europe/Warsaw", "en_GB").
    pub fn fixed(zone: &str, locale: &str) -> Cal {
        // SAFETY: a framework constant.
        let cal = NSCalendar::calendarWithIdentifier(unsafe { NSCalendarIdentifierGregorian }).expect("gregorian calendar");
        if let Some(tz) = NSTimeZone::timeZoneWithName(&NSString::from_str(zone)) {
            cal.setTimeZone(&tz);
        }
        Cal { cal, locale: Some(NSLocale::localeWithLocaleIdentifier(&NSString::from_str(locale))) }
    }

    fn date(t: f64) -> Retained<NSDate> {
        NSDate::dateWithTimeIntervalSince1970(t)
    }

    pub fn start_of_day(&self, t: f64) -> f64 {
        self.cal.startOfDayForDate(&Self::date(t)).timeIntervalSince1970()
    }

    /// Calendar days from `now`'s day to `due`'s day.
    pub fn days(&self, now: f64, due: f64) -> i64 {
        let a = self.cal.startOfDayForDate(&Self::date(now));
        let b = self.cal.startOfDayForDate(&Self::date(due));
        let c = self.cal.components_fromDate_toDate_options(NSCalendarUnit::Day, &a, &b, NSCalendarOptions::empty());
        c.day() as i64
    }

    pub fn hour_minute(&self, t: f64) -> (i64, i64) {
        let d = Self::date(t);
        (self.cal.component_fromDate(NSCalendarUnit::Hour, &d) as i64, self.cal.component_fromDate(NSCalendarUnit::Minute, &d) as i64)
    }

    pub fn add_days(&self, n: i64, t: f64) -> Option<f64> {
        self.cal
            .dateByAddingUnit_value_toDate_options(NSCalendarUnit::Day, n as isize, &Self::date(t), NSCalendarOptions::empty())
            .map(|d| d.timeIntervalSince1970())
    }

    /// The same day as `t`, at h:m:s (the next valid time when the clock skips it).
    pub fn at_time(&self, h: i64, m: i64, s: i64, t: f64) -> Option<f64> {
        self.cal
            .dateBySettingHour_minute_second_ofDate_options(h as isize, m as isize, s as isize, &Self::date(t), NSCalendarOptions::MatchNextTime)
            .map(|d| d.timeIntervalSince1970())
    }

    /// A local wall-clock time.
    pub fn local(&self, y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Option<f64> {
        let c = NSDateComponents::new();
        c.setYear(y as isize);
        c.setMonth(mo as isize);
        c.setDay(d as isize);
        c.setHour(h as isize);
        c.setMinute(mi as isize);
        c.setSecond(s as isize);
        self.cal.dateFromComponents(&c).map(|d| d.timeIntervalSince1970())
    }

    /// Day and short month in the locale's order ("26 Oct", "Oct 26").
    pub fn day_month(&self, t: f64) -> String {
        let f = NSDateFormatter::new();
        let loc = self.locale.clone().unwrap_or_else(NSLocale::autoupdatingCurrentLocale);
        f.setLocale(Some(&loc));
        f.setCalendar(Some(&self.cal));
        f.setTimeZone(Some(&self.cal.timeZone()));
        f.setLocalizedDateFormatFromTemplate(&NSString::from_str("dMMM"));
        f.stringFromDate(&Self::date(t)).to_string()
    }

    /// The short weekday name in the locale ("Sat").
    pub fn weekday(&self, t: f64) -> String {
        let f = NSDateFormatter::new();
        let loc = self.locale.clone().unwrap_or_else(NSLocale::autoupdatingCurrentLocale);
        f.setLocale(Some(&loc));
        f.setCalendar(Some(&self.cal));
        f.setTimeZone(Some(&self.cal.timeZone()));
        f.setLocalizedDateFormatFromTemplate(&NSString::from_str("EEE"));
        f.stringFromDate(&Self::date(t)).to_string()
    }
}

pub struct Due;

impl Due {
    /// A `due` value as epoch seconds: a number, or a numeric string. Anything else is no due time.
    pub fn seconds(v: Option<&Value>) -> Option<f64> {
        let x = match v? {
            Value::Number(n) => n.as_f64()?,
            Value::String(s) => s.trim_matches(|c| c == ' ' || c == '\t').parse::<f64>().ok()?,
            _ => return None,
        };
        (x.is_finite() && x > 0.0).then_some(x)
    }

    fn clock(t: f64, cal: &Cal) -> String {
        let (h, m) = cal.hour_minute(t);
        format!("{h:02}:{m:02}")
    }

    /// The label for an item due at `due`, and whether it is soon (within the hour: drawn at full white).
    /// Under an hour "in 12 min"; later today "in 3 h"; tomorrow "tomorrow 10:00"; 2–6 days "in 2 days"; then "26 Oct".
    pub fn label(due: f64, now: f64, cal: &Cal) -> (String, bool) {
        let d = due - now;
        if d <= 0.0 {
            return ("now".into(), true);
        }
        let mins = (d / 60.0).ceil() as i64;
        if mins < 60 {
            return (format!("in {} min", mins.max(1)), true);
        }
        match cal.days(now, due) {
            n if n <= 0 => (format!("in {} h", ((d / 3600.0).round() as i64).max(1)), false),
            1 => (format!("tomorrow {}", Self::clock(due, cal)), false),
            n @ 2..=6 => (format!("in {n} days"), false),
            _ => (cal.day_month(due), false),
        }
    }

    /// The next moment `label` reads differently (or the item falls due); `None` once it is due.
    pub fn next_change(due: f64, now: f64, cal: &Cal) -> Option<f64> {
        let d = due - now;
        if d <= 0.0 {
            return None;
        }
        let mins = (d / 60.0).ceil() as i64;
        if mins < 60 {
            return Some(due - (mins - 1) as f64 * 60.0);
        }
        let day_diff = cal.days(now, due);
        // Minutes take over once the rounded-up minutes drop below 60: 59 min before it is due.
        let into_hour = due - 3540.0;
        let midnight = cal.add_days(1, cal.start_of_day(now)).unwrap_or(into_hour);
        if day_diff <= 0 {
            let h = ((d / 3600.0).round() as i64).max(1);
            return Some(due - 3540f64.max((h as f64 - 0.5) * 3600.0));
        }
        Some(midnight.min(into_hour))
    }

    /// The ids of a Scheduled island's items that are due at `now` (not ticked): they stay in Scheduled, drawn as due
    /// now (what has to be done because it is scheduled lives in Scheduled; they no longer move to
    /// To do or Goals). The app writes one `due` response for each, once.
    pub fn due_now(scheduled: &Json, now: f64) -> Vec<String> {
        Plan::items(scheduled).into_iter().filter(|i| !i.done && i.due.is_some_and(|d| d <= now)).map(|i| i.id.unwrap_or(i.key)).collect()
    }

    /// The due items (not ticked) not yet told for their current due time, as (id, due): Heimdall stamps `told: <due>`
    /// on an item when it handles its `due` response (re-review 31 Oct: an in-memory set alone re-announced every stale
    /// overdue item at each start). The islands app only reads the stamp, never writes it; a rescheduled item (another
    /// `due`) is told again when it falls due.
    pub fn untold(scheduled: &Json, now: f64) -> Vec<(String, f64)> {
        let raw = super::j::arr(scheduled.get("items")).cloned().unwrap_or_default();
        Plan::items(scheduled)
            .into_iter()
            .filter(|i| !i.done)
            .filter_map(|i| {
                let due = i.due.filter(|d| *d <= now)?;
                let told = raw.get(i.index).and_then(|v| Self::seconds(v.get("told")));
                (told != Some(due)).then(|| (i.id.unwrap_or(i.key), due))
            })
            .collect()
    }

    /// A due time as written by hand or by the CLI, relative to `now` in `cal`'s time zone: "2026-10-24 10:00",
    /// "2026-10-24T10:00", an ISO 8601 date-time with an offset, "+30m" / "+2h" / "+3d", "today 22:00",
    /// "tomorrow 10:00", "tonight" (23:00 today). `None` when unreadable.
    pub fn parse(text: &str, now: f64, cal: &Cal) -> Option<f64> {
        let trimmed = text.trim_matches(|c| c == ' ' || c == '\t');
        let s = trimmed.to_lowercase();
        if let Some(body) = s.strip_prefix('+') {
            let unit = body.chars().last()?;
            let n: f64 = body[..body.len() - unit.len_utf8()].parse().ok()?;
            if n < 0.0 {
                return None;
            }
            let k = match unit {
                'm' => 60.0,
                'h' => 3600.0,
                'd' => 86400.0,
                _ => return None,
            };
            return Some(now + n * k);
        }
        if s == "tonight" {
            return cal.at_time(23, 0, 0, now);
        }
        let parts: Vec<&str> = s.split(' ').filter(|p| !p.is_empty()).collect();
        if parts.len() == 2 && ["today", "tomorrow", "tonight"].contains(&parts[0]) {
            let hm: Vec<i64> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();
            if hm.len() != 2 || !(0..24).contains(&hm[0]) || !(0..60).contains(&hm[1]) {
                return None;
            }
            let day = if parts[0] == "tomorrow" { cal.add_days(1, now).unwrap_or(now) } else { now };
            return cal.at_time(hm[0], hm[1], 0, day);
        }
        if let Some(t) = time::parse_iso8601(trimmed) {
            return Some(t);
        }
        // yyyy-MM-dd HH:mm[:ss] and yyyy-MM-dd'T'HH:mm[:ss], local.
        let b = trimmed.as_bytes();
        if (b.len() == 16 || b.len() == 19) && b[4] == b'-' && b[7] == b'-' && (b[10] == b' ' || b[10] == b'T') && b[13] == b':' {
            if b.len() == 19 && b[16] != b':' {
                return None;
            }
            let n = |r: std::ops::Range<usize>| trimmed.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<i64>().ok());
            let (y, mo, d, h, mi) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?);
            let se = if b.len() == 19 { n(17..19)? } else { 0 };
            if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 59 {
                return None;
            }
            return cal.local(y, mo, d, h, mi, se);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    fn cal() -> Cal {
        Cal::fixed("Europe/Warsaw", "en_GB")
    }
    fn at(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> f64 {
        cal().local(y, mo, d, h, mi, s).unwrap()
    }
    fn label(due: f64, now: f64) -> String {
        Due::label(due, now, &cal()).0
    }

    #[test]
    fn labels_count_down_in_the_readers_units() {
        let now = at(2026, 10, 23, 9, 0, 0);
        assert_eq!(label(at(2026, 10, 23, 9, 12, 0), now), "in 12 min");
        assert_eq!(label(at(2026, 10, 23, 9, 0, 20), now), "in 1 min");
        assert_eq!(label(at(2026, 10, 23, 9, 59, 30), now), "in 1 h", "59.5 min rounds up to the hour");
        assert_eq!(label(at(2026, 10, 23, 12, 10, 0), now), "in 3 h");
        assert_eq!(label(at(2026, 10, 23, 23, 0, 0), now), "in 14 h");
        assert_eq!(label(at(2026, 10, 24, 10, 0, 0), now), "tomorrow 10:00");
        assert_eq!(label(at(2026, 10, 25, 8, 0, 0), now), "in 2 days");
        assert_eq!(label(at(2026, 10, 29, 8, 0, 0), now), "in 6 days");
        assert_eq!(label(at(2026, 10, 30, 8, 0, 0), now), "30 Oct");
        assert_eq!(label(at(2026, 10, 23, 8, 0, 0), now), "now");
        assert!(Due::label(at(2026, 10, 23, 9, 30, 0), now, &cal()).1);
        assert!(!Due::label(at(2026, 10, 23, 12, 0, 0), now, &cal()).1);
        assert_eq!(label(at(2026, 10, 24, 6, 0, 0), at(2026, 10, 23, 23, 50, 0)), "tomorrow 06:00");
        assert_eq!(label(at(2026, 10, 24, 0, 20, 0), at(2026, 10, 23, 23, 50, 0)), "in 30 min");
    }

    #[test]
    fn next_change_is_exactly_when_the_label_changes() {
        // Walk across the DST change (25 Oct in Warsaw): the label never changes before next_change, and does at it.
        let due = at(2026, 10, 25, 7, 30, 0);
        let mut now = at(2026, 10, 23, 18, 0, 17);
        let mut steps = 0;
        while now < due {
            let next = Due::next_change(due, now, &cal()).unwrap();
            assert!(next > now);
            let before = next - 0.5;
            if before > now {
                assert_eq!(label(due, before), label(due, now), "{now} → {next}");
            }
            assert_ne!(label(due, next + 0.01), label(due, now), "{now} → {next}");
            now = next + 0.01;
            steps += 1;
        }
        assert!(steps < 120, "the timer wakes rarely: {steps} changes in a day and a half");
        assert_eq!(Due::next_change(due, due + 1.0, &cal()), None);
    }

    #[test]
    fn the_timer_sleeps_long_when_nothing_is_near() {
        let now = at(2026, 10, 23, 9, 0, 0);
        assert_eq!(Due::next_change(at(2026, 10, 30, 9, 0, 0), now, &cal()), Some(at(2026, 10, 24, 0, 0, 0)));
        assert_eq!(Due::next_change(at(2026, 10, 23, 14, 0, 0), now, &cal()), Some(at(2026, 10, 23, 9, 30, 0)));
    }

    #[test]
    fn due_items_stay_in_scheduled_and_come_first() {
        // The real clock: `Plan::grouped` reads it to put what is due now first.
        let now = time::now();
        let sched = jobj! {"id": "scheduled", "list": "scheduled", "items": [
            {"id": "b", "title": "Later", "due": now + 3600.0},
            {"id": "a", "title": "Arsenal: push", "due": now - 60.0},
            {"id": "c", "title": "Ticked", "due": now - 60.0, "done": true},
            {"title": "No id", "due": now - 1.0},
            "Plain"]};
        assert_eq!(Due::due_now(&sched, now), ["a", "No id"]);
        // Drawn first, the rest in file order, ticked last.
        let order: Vec<String> = Plan::display_order(&sched, "scheduled").into_iter().map(|i| i.key).collect();
        assert_eq!(order, ["a", "No id", "b", "Plain", "c"]);
    }

    #[test]
    fn a_due_item_stamped_told_for_its_due_time_is_not_told_again() {
        let now = 2_000_000_000.0;
        let mut s = jobj! {"id": "scheduled", "items": [
            {"id": "a", "title": "A", "due": now - 60.0},
            {"id": "b", "title": "B", "due": now + 60.0},
            {"id": "c", "title": "C", "due": now - 60.0, "done": true},
            {"id": "d", "title": "D", "due": "1999999000", "told": 1999999000}]};
        let before = s.clone();
        assert_eq!(Due::untold(&s, now), [("a".to_owned(), now - 60.0)]);
        assert_eq!(s, before, "the islands side never writes the stamp");
        // Heimdall stamped it: a restart tells nothing.
        s["items"][0]["told"] = serde_json::json!(now - 60.0);
        assert!(Due::untold(&s, now).is_empty());
        // Rescheduled: told again when the new time comes.
        s["items"][0]["due"] = serde_json::json!(now + 9000.0);
        assert_eq!(Due::untold(&s, now + 9000.0).len(), 2, "a again, and b now due");
    }

    #[test]
    fn plan_items_read_due() {
        let items = Plan::items(&jobj! {"items": [
            {"id": "a", "title": "A", "due": 1792700000},
            {"id": "b", "title": "B", "due": "1792700000.5"},
            {"id": "c", "title": "C", "due": true},
            {"id": "d", "title": "D", "due": "Fri"}]});
        assert_eq!(items.iter().map(|i| i.due).collect::<Vec<_>>(), [Some(1792700000.0), Some(1792700000.5), None, None]);
    }

    #[test]
    fn parse_hand_written_times() {
        let now = at(2026, 10, 23, 9, 0, 0);
        let p = |s: &str| Due::parse(s, now, &cal());
        assert_eq!(p("2026-10-24 10:00"), Some(at(2026, 10, 24, 10, 0, 0)));
        assert_eq!(p("2026-10-24T10:00"), Some(at(2026, 10, 24, 10, 0, 0)));
        assert_eq!(p("2026-10-24T08:00:00Z"), Some(at(2026, 10, 24, 10, 0, 0)));
        assert_eq!(p("+2h"), Some(now + 7200.0));
        assert_eq!(p("+30m"), Some(now + 1800.0));
        assert_eq!(p("+3d"), Some(now + 3.0 * 86400.0));
        assert_eq!(p("tomorrow 10:00"), Some(at(2026, 10, 24, 10, 0, 0)));
        assert_eq!(p("today 22:00"), Some(at(2026, 10, 23, 22, 0, 0)));
        assert_eq!(p("Tonight"), Some(at(2026, 10, 23, 23, 0, 0)));
        assert_eq!(p("Fri"), None);
        assert_eq!(p("tomorrow 25:00"), None);
    }
}
