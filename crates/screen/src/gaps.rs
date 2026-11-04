//! Diagnosis only (bug 2026-10-24T02-35-47: 1–4 s archive gaps during fullscreen video, nothing logged): one log line
//! per display when more than `GAP_NS` passed between two frames that reached the archive, naming what macOS sent
//! meanwhile (SCFrameStatus per callback, or no callback at all). At most one line per `LINE_EVERY_NS`; the gaps in
//! between are counted into the next line. Changes nothing about capture.

pub(crate) const GAP_NS: u64 = 1_000_000_000;
pub(crate) const LINE_EVERY_NS: u64 = 60_000_000_000;

/// One SCK screen callback, as the capture path handled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// A complete or idle frame reached the buffer.
    Kept,
    /// SCFrameStatus other than complete or idle (2 blank, 3 suspended, 4 started, 5 stopped), or none attached.
    Status(Option<isize>),
    /// The display was not to be recorded (locked session, asleep, excluded).
    Refused,
    /// The frame failed on the way in (timestamp, pixels, write).
    Failed,
    /// A stream from an ended capture generation.
    Stale,
}

#[derive(Default)]
pub(crate) struct GapLog {
    last_kept: Option<u64>,
    last_line: Option<u64>,
    /// Gaps over `GAP_NS` not logged since the last line.
    held: u32,
    callbacks: u32,
    blank: u32,
    suspended: u32,
    started: u32,
    stopped: u32,
    no_status: u32,
    other: u32,
    refused: u32,
    failed: u32,
    stale: u32,
}

impl GapLog {
    /// Note one callback at `now` (host ns, callback receipt); the line to log when it closes a gap.
    pub(crate) fn observe(&mut self, display: u32, now: u64, seen: Seen) -> Option<String> {
        self.callbacks += 1;
        match seen {
            Seen::Kept => {}
            Seen::Status(Some(2)) => self.blank += 1,
            Seen::Status(Some(3)) => self.suspended += 1,
            Seen::Status(Some(4)) => self.started += 1,
            Seen::Status(Some(5)) => self.stopped += 1,
            Seen::Status(None) => self.no_status += 1,
            Seen::Status(Some(_)) => self.other += 1,
            Seen::Refused => self.refused += 1,
            Seen::Failed => self.failed += 1,
            Seen::Stale => self.stale += 1,
        }
        if seen != Seen::Kept { return None }
        let line = match self.last_kept {
            Some(last) if now.saturating_sub(last) > GAP_NS => {
                if self.last_line.is_some_and(|l| now.saturating_sub(l) < LINE_EVERY_NS) {
                    self.held += 1;
                    None
                } else {
                    let line = self.line(display, now - last);
                    self.last_line = Some(now);
                    self.held = 0;
                    Some(line)
                }
            }
            _ => None,
        };
        let held = self.held;
        *self = GapLog { last_kept: Some(now), last_line: self.last_line, held, ..GapLog::default() };
        line
    }

    fn line(&self, display: u32, gap: u64) -> String {
        // The callbacks in between, without the one that ended the gap.
        let between = self.callbacks - 1;
        let what = if between == 0 {
            "macOS delivered no frame callbacks in between (none sent, or dropped before delivery)".to_owned()
        } else {
            let parts: Vec<String> = [
                ("blank", self.blank), ("suspended", self.suspended), ("started", self.started), ("stopped", self.stopped),
                ("no status", self.no_status), ("other status", self.other), ("display not recorded", self.refused),
                ("failed on the way in", self.failed), ("old capture generation", self.stale),
            ].iter().filter(|(_, n)| *n > 0).map(|(k, n)| format!("{k} {n}")).collect();
            format!("{between} callbacks in between: {}", parts.join(", "))
        };
        let held = if self.held > 0 { format!(" (+{} earlier gaps over 1 s not logged)", self.held) } else { String::new() };
        format!("screen capture gap: display {display} kept no frame for {:.1} s; {what}{held}", gap as f64 / 1e9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;

    #[test]
    fn a_gap_over_a_second_logs_once_with_what_macos_sent() {
        let mut g = GapLog::default();
        // Steady 5 fps: nothing logged.
        for i in 0..10 { assert_eq!(g.observe(1, i * S / 5, Seen::Kept), None); }
        // 2.4 s with two blank and one suspended frame, then a frame again.
        let t = 9 * S / 5;
        assert_eq!(g.observe(1, t + S / 2, Seen::Status(Some(2))), None);
        assert_eq!(g.observe(1, t + S, Seen::Status(Some(2))), None);
        assert_eq!(g.observe(1, t + 2 * S, Seen::Status(Some(3))), None);
        assert_eq!(g.observe(1, t + 12 * S / 5, Seen::Kept).as_deref(),
            Some("screen capture gap: display 1 kept no frame for 2.4 s; 3 callbacks in between: blank 2, suspended 1"));
        // A second at exactly the limit is no gap.
        assert_eq!(g.observe(1, t + 17 * S / 5, Seen::Kept), None);
    }

    #[test]
    fn gaps_within_a_minute_of_a_line_are_counted_into_the_next_one() {
        let mut g = GapLog::default();
        g.observe(1, 0, Seen::Kept);
        assert!(g.observe(1, 2 * S, Seen::Kept).unwrap().ends_with("macOS delivered no frame callbacks in between (none sent, or dropped before delivery)"));
        // Seven more gaps inside the minute: no line each.
        for i in 0..7 { assert_eq!(g.observe(1, 4 * S + i * 3 * S, Seen::Kept), None); }
        // Frames a second apart (no gap) until the minute is over, then a gap with the display refused meanwhile.
        for t in 23..=64 { assert_eq!(g.observe(1, t * S, Seen::Kept), None); }
        g.observe(1, 65 * S, Seen::Refused);
        let line = g.observe(1, 67 * S, Seen::Kept).unwrap();
        assert!(line.contains("for 3.0 s; 1 callbacks in between: display not recorded 1 (+7 earlier gaps over 1 s not logged)"), "{line}");
        // The count starts over after a line.
        assert_eq!(g.observe(1, 68 * S, Seen::Kept), None);
        assert!(!g.observe(1, 140 * S, Seen::Kept).unwrap().contains("earlier gaps"));
    }
}
