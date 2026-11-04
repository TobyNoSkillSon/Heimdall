//! A spent plan limit or prepaid balance, read from what T3 Code shows for a failed run (bug note
//! 2026-11-02T14-47-37). On 2 Nov every relay failure was logged as "failed 3 requests in a row; the last error: run
//! failed" while T3 had the real reason. What T3 0.1.6 keeps for such a run (`t3_thread_read`, activity view, read on
//! the live server 2 Nov):
//! - a `system_notice` item whose id ends `usage-limit:…:<window>:<reset, Unix seconds>` (`seven_day`, `five_hour`)
//!   and whose text is "Claude usage limit reached. This turn is paused until the 7-day limit resets in 6h 31m.";
//! - Claude Code's reply "You've hit your weekly limit · resets 1pm (Europe/Warsaw)" or "… · resets Nov 6 at 9am";
//! - an `error` item, "Usage limit reached: Claude API rate limit reached. Try again later." or "Provider error: API
//!   error (status 402 Payment Required): Grok Build usage balance exhausted".
//!
//! `parse` takes any of these (the controller joins a run's items into one text) and says which window is spent and
//! when it resets: the id's exact reset first, then the stated clock time, then the relative one. Pure.
use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone};

/// What ran out, and when it is back when the provider said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limit {
    /// "weekly", "5-hour", or `None` when the text names no window.
    pub window: Option<String>,
    pub reset_ms: Option<u64>,
    /// A prepaid balance or credit (402 Payment Required): it does not come back by itself.
    pub billing: bool,
}

/// The id T3 gives a usage-limit notice contains this, then `:<window>:<seconds>` at its end.
pub const MARKER: &str = "usage-limit";

impl Limit {
    /// One plain sentence: "Claude's weekly limit is used up; it resets at 13:00." `who` is the account's name.
    pub fn sentence(&self, who: &str, now_ms: u64) -> String {
        if self.billing { return format!("{who}'s prepaid balance is used up; it does not come back by itself."); }
        let what = match self.window.as_deref() { Some(w) => format!("{w} limit"), None => "usage limit".into() };
        match self.reset_ms {
            Some(at) => format!("{who}'s {what} is used up; it resets at {}.", when(at, now_ms)),
            None => format!("{who}'s {what} is used up; the provider gave no reset time."),
        }
    }
    /// The controller's cause for it (`Cause::Quota`, with this sentence).
    pub fn cause(&self, who: &str, now_ms: u64) -> crate::emergency::Cause {
        crate::emergency::Cause::Quota { reset_ms: self.reset_ms, line: Some(self.sentence(who, now_ms)) }
    }
}

/// "13:00" today (or within the next 20 hours), else "Thu 09:00".
pub fn when(at_ms: u64, now_ms: u64) -> String {
    let Some(t) = Local.timestamp_millis_opt(at_ms as i64).single() else { return "?".into() };
    if at_ms.saturating_sub(now_ms) < 20 * 3_600_000 { t.format("%H:%M").to_string() } else { t.format("%a %H:%M").to_string() }
}

/// The limit `text` reports, `None` when it is not a spent limit or balance (a passing error, a crash).
pub fn parse(text: &str, now_ms: u64) -> Option<Limit> {
    let lower = text.to_ascii_lowercase();
    let billing = ["402 payment required", "status 402", "balance exhausted", "credit balance", "insufficient balance", "insufficient credit", "insufficient_quota"]
        .iter().any(|p| lower.contains(p));
    let marker = marker(text);
    let spent = billing || marker.is_some() || ["you've hit your", "you have hit your", "usage limit", "usage_limit", "limit reached|"].iter().any(|p| lower.contains(p));
    if !spent { return None; }
    let window = marker.as_ref().and_then(|(w, _)| w.clone()).or_else(|| window_of(&lower));
    let reset_ms = marker.and_then(|(_, r)| r).or_else(|| stated(&lower, now_ms)).or_else(|| relative(&lower, now_ms)).or_else(|| piped(text));
    Some(Limit { window, reset_ms, billing: billing && reset_ms.is_none() })
}

/// `…usage-limit:…:seven_day:1793620800` in a T3 item id (percent-encoded or not): the window and the reset.
fn marker(text: &str) -> Option<(Option<String>, Option<u64>)> {
    // The newest id; only an id ("usage-limit%3A…" or "usage-limit:…"), never the word in prose.
    let start = text.rmatch_indices(MARKER).map(|(i, _)| i).find(|&i| { let rest = &text[i + MARKER.len()..]; rest.starts_with("%3A") || rest.starts_with("%3a") || rest.starts_with(':') })?;
    let id: String = text[start..].chars().take_while(|c| !c.is_whitespace() && !matches!(c, ']' | '"' | ',' | ')')).collect();
    let id = id.replace("%3A", ":").replace("%3a", ":");
    let parts: Vec<&str> = id.split(':').collect();
    let secs = parts.last().and_then(|s| s.parse::<u64>().ok()).filter(|s| *s > 1_000_000_000);
    let window = (parts.len() >= 2).then(|| parts[parts.len() - 2]).and_then(window_name);
    Some((window, secs.map(|s| if s < 100_000_000_000 { s * 1000 } else { s })))
}
fn window_name(raw: &str) -> Option<String> {
    let r = raw.to_ascii_lowercase();
    if r.contains("seven") || r.contains("week") || r.contains("7_day") { Some("weekly".into()) }
    else if r.contains("five") || r.contains("5_hour") || r.contains("session") { Some("5-hour".into()) }
    else { None }
}
fn window_of(lower: &str) -> Option<String> {
    if lower.contains("weekly") || lower.contains("7-day") || lower.contains("seven-day") || lower.contains("week limit") { Some("weekly".into()) }
    else if lower.contains("5-hour") || lower.contains("five-hour") || lower.contains("5 hour") || lower.contains("session limit") { Some("5-hour".into()) }
    else if lower.contains("monthly") { Some("monthly".into()) }
    else { None }
}
/// "resets 1pm", "resets at 13:00", "resets Nov 6 at 9am", "resets 9:30am (Europe/Warsaw)": the next such moment in
/// local time (the Mac's zone, which Claude Code names in brackets).
fn stated(lower: &str, now_ms: u64) -> Option<u64> {
    let i = lower.find("resets ")?;
    let rest = lower[i + 7..].trim_start();
    if rest.starts_with("in ") { return None; }
    let rest = rest.strip_prefix("at ").unwrap_or(rest);
    let words: Vec<&str> = rest.split_whitespace().take(5).collect();
    // A date first ("nov 6 at 9am", "6 nov 9am")?
    let (date, time_word) = match words.as_slice() {
        [m, d, "at", t, ..] | [m, d, t, ..] if month(m).is_some() && day(d).is_some() => (Some((month(m)?, day(d)?)), *t),
        [d, m, "at", t, ..] | [d, m, t, ..] if month(m).is_some() && day(d).is_some() => (Some((month(m)?, day(d)?)), *t),
        [t, ..] => (None, *t),
        [] => return None,
    };
    let time = clock_time(time_word)?;
    let now = Local.timestamp_millis_opt(now_ms as i64).single()?;
    let at = |d: NaiveDate| Local.from_local_datetime(&d.and_time(time)).earliest().map(|t| t.timestamp_millis() as u64);
    match date {
        Some((m, d)) => {
            for year in [now.year(), now.year() + 1] {
                if let Some(t) = NaiveDate::from_ymd_opt(year, m, d).and_then(at) { if t + 86_400_000 > now_ms { return Some(t); } }
            }
            None
        }
        None => {
            let today = at(now.date_naive())?;
            if today + 60_000 > now_ms { Some(today) } else { now.date_naive().succ_opt().and_then(at) }
        }
    }
}
fn month(w: &str) -> Option<u32> {
    let w = w.trim_end_matches(['.', ',']);
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"].iter().position(|m| w.starts_with(m)).map(|p| p as u32 + 1)
}
fn day(w: &str) -> Option<u32> {
    let digits: String = w.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}
/// "1pm", "9am", "9:30am", "13:00", "1 pm" is not taken (the next word is not read).
fn clock_time(w: &str) -> Option<NaiveTime> {
    let w = w.trim_end_matches(['.', ',', ';']);
    let (body, pm) = if let Some(b) = w.strip_suffix("pm") { (b, Some(true)) } else if let Some(b) = w.strip_suffix("am") { (b, Some(false)) } else { (w, None) };
    let (h, m) = match body.split_once(':') { Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?), None => (body.parse::<u32>().ok()?, 0) };
    let h = match pm { Some(true) if h < 12 => h + 12, Some(false) if h == 12 => 0, None if !body.contains(':') => return None, _ => h };
    NaiveTime::from_hms_opt(h, m, 0)
}
/// "resets in 6h 31m", "resets in 95h 15m", "resets in 45m", "resets in 2d".
fn relative(lower: &str, now_ms: u64) -> Option<u64> {
    let i = lower.find("resets in ")?;
    let mut total = 0u64;
    let mut any = false;
    for word in lower[i + 10..].split_whitespace().take(4) {
        let digits: String = word.chars().take_while(char::is_ascii_digit).collect();
        let Ok(n) = digits.parse::<u64>() else { break };
        let unit = &word[digits.len()..];
        total += n * match unit.trim_end_matches(['.', ',']) { "d" | "day" | "days" => 86_400_000, "h" | "hr" | "hours" | "hour" => 3_600_000, "m" | "min" | "mins" | "minutes" => 60_000, _ => break };
        any = true;
    }
    any.then_some(now_ms + total)
}
/// Claude Code's older "Claude AI usage limit reached|1793066649" (Unix seconds).
fn piped(text: &str) -> Option<u64> {
    let (_, rest) = text.split_once("reached|")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: u64 = digits.parse().ok()?;
    Some(if n < 100_000_000_000 { n * 1000 } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> u64 {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap().timestamp_millis() as u64
    }

    /// The three failures of 2 Nov, as T3 0.1.6 keeps them (threads c920c7d1, c6c1ea41, 57afd961, read live).
    #[test]
    fn the_three_failures_of_2_november_read_as_limits_with_their_resets() {
        let at_0629 = local(2026, 11, 2, 6, 29);
        // Claude main: the notice's id carries the exact reset (1793620800 s = 13:00 CET), the reply the clock time.
        let main = "You've hit your weekly limit · resets 1pm (Europe/Warsaw)\nUsage limit reached: Claude API rate limit reached. Try again later.\nClaude usage limit reached. This turn is paused until the 7-day limit resets in 6h 31m. [turn-item:provider:claudeAgent:native-item:usage-limit%3Aprovider-turn%3Aprovider%3AclaudeAgent%3Anative-turn%3Aturn%253Arun-attempt%3Aseven_day%3A1793620800]";
        let l = parse(main, at_0629).unwrap();
        assert_eq!(l, Limit { window: Some("weekly".into()), reset_ms: Some(1_793_620_800_000), billing: false });
        // Without the id: the stated 1pm, today.
        let l = parse("You've hit your weekly limit · resets 1pm (Europe/Warsaw)", at_0629).unwrap();
        assert_eq!((l.window.as_deref(), l.reset_ms), (Some("weekly"), Some(local(2026, 11, 2, 13, 0))));
        assert_eq!(l.sentence("Claude", at_0629), "Claude's weekly limit is used up; it resets at 13:00.");
        // Claude 2nd: a date.
        let at_0945 = local(2026, 11, 2, 9, 45);
        let l = parse("You've hit your weekly limit · resets Nov 6 at 9am (Europe/Warsaw)", at_0945).unwrap();
        assert_eq!(l.reset_ms, Some(local(2026, 11, 6, 9, 0)));
        assert!(l.sentence("Claude code 2", at_0945).ends_with("it resets at Fri 09:00."), "{}", l.sentence("Claude code 2", at_0945));
        // Only the relative notice.
        let l = parse("Claude usage limit reached. This turn is paused until the 7-day limit resets in 95h 15m.", at_0945).unwrap();
        assert_eq!((l.window.as_deref(), l.reset_ms), (Some("weekly"), Some(at_0945 + 95 * 3_600_000 + 15 * 60_000)));
        // Grok: a prepaid balance, no reset.
        let l = parse("Provider error: API error (status 402 Payment Required): Grok Build usage balance exhausted", at_0945).unwrap();
        assert_eq!(l, Limit { window: None, reset_ms: None, billing: true });
        assert_eq!(l.sentence("Grok", at_0945), "Grok's prepaid balance is used up; it does not come back by itself.");
        assert!(matches!(l.cause("Grok", 0), crate::emergency::Cause::Quota { reset_ms: None, line: Some(_) }));
    }

    #[test]
    fn passing_errors_are_not_limits_and_a_passed_clock_time_is_tomorrow() {
        assert_eq!(parse("run failed", 0), None);
        assert_eq!(parse("API Error: 529 overloaded_error", 0), None);
        assert_eq!(parse("Claude API rate limit reached. Try again later.", 0), None, "a bare rate limit can pass by itself");
        assert_eq!(parse("I checked the usage-limit docs for you.", 0), None, "the word in prose is no id");
        let late = local(2026, 11, 2, 14, 0);
        assert_eq!(parse("You've hit your session limit · resets 1pm", late).unwrap().reset_ms, Some(local(2026, 11, 3, 13, 0)));
        assert_eq!(parse("You've hit your limit · resets 13:30", late).unwrap().reset_ms, Some(local(2026, 11, 3, 13, 30)));
        assert_eq!(parse("Claude AI usage limit reached|1793066649", 0).unwrap().reset_ms, Some(1_793_066_649_000));
        assert_eq!(parse("You've hit your 5-hour limit · resets 12am", late).unwrap().reset_ms, Some(local(2026, 11, 3, 0, 0)));
    }
}
