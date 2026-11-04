//! Wall-clock time as epoch seconds, and ISO 8601 as `ISO8601DateFormatter` writes and reads it
//! (`2026-10-23T08:00:00Z`; offsets accepted on read).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `ISO8601DateFormatter().string(from:)`: UTC, whole seconds.
pub fn iso8601(t: f64) -> String {
    let s = t.floor() as i64;
    let days = s.div_euclid(86_400);
    let rem = s.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// `ISO8601DateFormatter().date(from:)`: `YYYY-MM-DDTHH:MM:SS` with `Z` or `±HH:MM` (also `±HHMM`, `±HH`).
pub fn parse_iso8601(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<i64>().ok());
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let tz = &s[19..];
    let offset = if tz == "Z" || tz == "z" {
        0
    } else {
        let sign = match tz.as_bytes().first()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let digits: String = tz[1..].chars().filter(|c| *c != ':').collect();
        if !digits.bytes().all(|c| c.is_ascii_digit()) || !(digits.len() == 2 || digits.len() == 4) {
            return None;
        }
        let oh: i64 = digits[..2].parse().ok()?;
        let om: i64 = if digits.len() == 4 { digits[2..].parse().ok()? } else { 0 };
        sign * (oh * 3600 + om * 60)
    };
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trips_and_reads_offsets() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_792_596_000.7), "2026-10-21T15:20:00Z");
        assert_eq!(parse_iso8601("2026-10-21T15:20:00Z"), Some(1_792_596_000.0));
        assert_eq!(parse_iso8601("2026-10-24T10:00:00+02:00"), parse_iso8601("2026-10-24T08:00:00Z"));
        assert_eq!(parse_iso8601("2026-10-24T10:00:00+0200"), parse_iso8601("2026-10-24T08:00:00Z"));
        assert_eq!(parse_iso8601("2026-10-24 10:00"), None);
        assert_eq!(parse_iso8601("soon"), None);
    }
}
