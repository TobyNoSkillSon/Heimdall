//! Replies the user has probably not read yet (bug note 22 Oct 15:13: "you've responded twice and I cannot see your last
//! response"). With Speaking off a final reply shows as text: an island line for its read time, and the notch's
//! pull-down. A second reply 7 s later took both places, and the first, about Mistral, was gone before he read it.
//!
//! Now a reply shown as text stays "probably unread" for the time it takes to read it (`reading_time`). A reply that
//! arrives inside that time does not wipe it: the pull-down shows the unread replies first and the new one last, and
//! the agent is told once, as context, which reply he probably missed. A spoken reply was heard, so it never counts.
use std::time::{Duration, Instant};

/// The reading speed the estimate assumes, in words a minute: ordinary on-screen reading while doing something else
/// (an assumption, not measured on the user).
const WPM: f64 = 200.0;
/// Time to notice the island and start reading.
const NOTICE_SECONDS: f64 = 2.0;
/// At most this many unread replies stay above the newest one in the pull-down (the oldest goes first).
const KEPT: usize = 3;
/// How much of a missed reply the agent's note quotes (it has the whole reply in its own history).
const QUOTE_CHARS: usize = 160;
/// The note's id prefix: a context notice, never a turn (`agent::notice::NoticeKind::classify`, `unheard-speech`).
pub(super) const UNREAD_NOTE: &str = "unheard-speech:unread";

/// How long the user needs to read `text` shown as text.
pub(super) fn reading_time(text: &str) -> Duration {
    let words = text.split_whitespace().count() as f64;
    Duration::from_secs_f64(NOTICE_SECONDS + words * 60.0 / WPM)
}

struct Shown {
    text: String,
    at: Instant,
    told: bool,
}

/// The replies shown as text whose reading time has not passed, oldest first.
#[derive(Default)]
pub(crate) struct Unread {
    shown: Vec<Shown>,
}

/// What a new reply does to the island and the agent.
#[derive(Debug, PartialEq)]
pub(super) struct Arrival {
    /// The pull-down's text: the unread replies, then this one (paragraph gaps between them).
    pub pull_down: String,
    /// The unread replies alone (what the pull-down keeps if this reply is withdrawn).
    pub earlier: Option<String>,
    /// Unread replies not told to the agent yet, with how long each was on screen before this one came.
    pub missed: Vec<(String, Duration)>,
}

impl Unread {
    /// A reply is about to go to the island at `now`.
    pub fn arrive(&mut self, text: &str, now: Instant) -> Arrival {
        self.shown.retain(|s| now < s.at + reading_time(&s.text));
        while self.shown.len() > KEPT {
            self.shown.remove(0);
        }
        let missed = self
            .shown
            .iter_mut()
            .filter(|s| !s.told)
            .map(|s| {
                s.told = true;
                (s.text.clone(), now.saturating_duration_since(s.at))
            })
            .collect();
        let earlier = (!self.shown.is_empty()).then(|| self.shown.iter().map(|s| s.text.trim()).collect::<Vec<_>>().join("\n\n"));
        let pull_down = match &earlier {
            Some(e) => format!("{e}\n\n{}", text.trim()),
            None => text.to_string(),
        };
        Arrival { pull_down, earlier, missed }
    }

    /// The reply went out as text (Speaking off, speech held or failed): the user has to read it.
    pub fn shown_as_text(&mut self, text: &str, now: Instant) {
        self.shown.push(Shown { text: text.trim().to_string(), at: now, told: false });
    }
}

/// The agent's note for replies the user probably did not read (context only: it decides whether to say them again).
pub(super) fn note(missed: &[(String, Duration)]) -> Option<String> {
    let quote = |text: &str| {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() <= QUOTE_CHARS {
            return text;
        }
        let cut: String = text.chars().take(QUOTE_CHARS).collect();
        let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head).trim_end_matches([',', ';', ':']);
        format!("{cut}…")
    };
    match missed {
        [] => None,
        [(text, on)] => Some(format!(
            "User has probably not read your earlier reply: it showed as text and your next reply followed {} s later, \
             before he could finish reading it. The notch's pull-down shows both, newest last. If it still matters or he \
             asks, say it again briefly. It began: \"{}\"",
            on.as_secs(),
            quote(text)
        )),
        many => Some(format!(
            "User has probably not read {} of your earlier replies: they showed as text and the next reply followed \
             before he could finish reading them. The notch's pull-down shows them, newest last. If they still matter \
             or he asks, say them again briefly. They began: {}",
            many.len(),
            many.iter().map(|(text, _)| format!("\"{}\"", quote(text))).collect::<Vec<_>>().join("; ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two replies of 22 Oct 15:12, as written.
    pub(super) const MISTRAL: &str = "I found it: it's the Mistral voice agent demo, with two people talking to Charlotte. I've started research on how it works. It runs three stages streaming at once: Voxtral transcribes, detects turns and labels speakers in real time; Mistral Large reasons along as people talk; and Voxtral's speech model answers, with the delivery shaped by context. The worker will work out the details, compare it with how I work, and bring back the top ideas we could adopt.";
    pub(super) const WEB: &str = "Right. I can fetch a page through the shell, but I have no proper search tool, so research always goes to a worker. There are two ways to fix it. I can add a web search and fetch tool to my own setup myself, which needs a search API key such as Brave or Exa. Or it goes into tonight's night pass, built into Heimdall as a proper tool. I'd go with tonight, using a provider you already have a key for. Do you have one?";

    /// 15:12:13 and 15:12:21: the second reply keeps the first in the pull-down, above it, and the agent learns
    /// the first was probably not read.
    #[test]
    fn a_reply_seconds_after_an_unread_one_keeps_it_and_tells_the_agent() {
        let t0 = Instant::now();
        let mut u = Unread::default();
        let first = u.arrive(MISTRAL, t0);
        assert_eq!(first, Arrival { pull_down: MISTRAL.into(), earlier: None, missed: vec![] });
        u.shown_as_text(MISTRAL, t0);
        let t1 = t0 + Duration::from_millis(7_500);
        let second = u.arrive(WEB, t1);
        assert_eq!(second.pull_down, format!("{MISTRAL}\n\n{WEB}"), "both, newest last");
        assert_eq!(second.earlier.as_deref(), Some(MISTRAL));
        assert_eq!(second.missed, vec![(MISTRAL.to_string(), Duration::from_millis(7_500))]);
        let note = note(&second.missed).unwrap();
        assert!(note.starts_with("User has probably not read your earlier reply") && note.contains("followed 7 s later"), "{note}");
        assert!(note.contains("It began: \"I found it: it's the Mistral voice agent demo"), "{note}");
        assert!(note.chars().count() < 520, "a short note: {note}");
        // Told once: a third reply soon after keeps both unread replies but tells only the new one.
        u.shown_as_text(WEB, t1);
        let third = u.arrive("Done.", t1 + Duration::from_secs(3));
        assert_eq!(third.pull_down, format!("{MISTRAL}\n\n{WEB}\n\nDone."));
        assert_eq!(third.missed.len(), 1);
        assert_eq!(third.missed[0].0, WEB);
    }

    /// A minute apart, each reply had its reading time: the pull-down shows only the new one, nothing is told.
    #[test]
    fn a_reply_a_minute_later_replaces_a_read_one() {
        assert!(reading_time(MISTRAL) < Duration::from_secs(60), "{:?}", reading_time(MISTRAL));
        assert!(reading_time(MISTRAL) > Duration::from_secs(20), "80 words take more than the island line's 8 s");
        let t0 = Instant::now();
        let mut u = Unread::default();
        u.arrive(MISTRAL, t0);
        u.shown_as_text(MISTRAL, t0);
        let later = u.arrive(WEB, t0 + Duration::from_secs(60));
        assert_eq!(later, Arrival { pull_down: WEB.into(), earlier: None, missed: vec![] });
        // A very long reply is still unread a minute later.
        let long = "word ".repeat(400);
        u.shown_as_text(&long, t0 + Duration::from_secs(60));
        let after = u.arrive(WEB, t0 + Duration::from_secs(120));
        assert_eq!(after.missed.len(), 1);
    }

    /// A spoken reply was heard: the next reply replaces it, as before.
    #[test]
    fn a_spoken_reply_is_never_unread() {
        let t0 = Instant::now();
        let mut u = Unread::default();
        u.arrive(MISTRAL, t0);
        let next = u.arrive(WEB, t0 + Duration::from_secs(2));
        assert_eq!(next, Arrival { pull_down: WEB.into(), earlier: None, missed: vec![] });
        assert_eq!(note(&[]), None);
    }

    /// A burst keeps the newest few unread replies (the pull-down never grows without bound).
    #[test]
    fn a_burst_keeps_the_newest_unread_replies() {
        let t0 = Instant::now();
        let mut u = Unread::default();
        for k in 0..6 {
            let text = format!("Reply {k} with a few more words to read.");
            u.arrive(&text, t0 + Duration::from_millis(k * 200));
            u.shown_as_text(&text, t0 + Duration::from_millis(k * 200));
        }
        let last = u.arrive("Last.", t0 + Duration::from_secs(2));
        assert_eq!(last.pull_down.matches("Reply ").count(), KEPT, "{}", last.pull_down);
        assert!(last.pull_down.starts_with("Reply 3") && last.pull_down.ends_with("Last."), "{}", last.pull_down);
        assert_eq!(last.missed.len(), 1, "each was told once, when the next came");
        // Two not yet told (shown without a reply between them, e.g. a skipped final): one note for both.
        let two = vec![("First one.".to_string(), Duration::from_secs(4)), ("Second one.".to_string(), Duration::from_secs(1))];
        let note = note(&two).unwrap();
        assert!(note.starts_with("User has probably not read 2 of your earlier replies") && note.ends_with("\"First one.\"; \"Second one.\""), "{note}");
    }

    /// A long reply is quoted by its start, cut at a word.
    #[test]
    fn the_note_quotes_the_start_of_a_long_reply() {
        let note = note(&[(MISTRAL.to_string(), Duration::from_secs(7))]).unwrap();
        let quoted = note.split("It began: \"").nth(1).unwrap().trim_end_matches('"');
        assert!(quoted.ends_with('…') && quoted.chars().count() <= QUOTE_CHARS + 1, "{quoted}");
        assert!(MISTRAL.starts_with(quoted.trim_end_matches('…')), "{quoted}");
    }
}

/// The whole path through `Tools::agent_final_answer`, as on 22 Oct 15:12 (Speaking off).
#[cfg(test)]
mod tools_tests {
    use super::tests::{MISTRAL, WEB};
    use std::sync::{atomic::Ordering, Arc};

    fn tools(dir: &std::path::Path) -> anyhow::Result<(crate::bridge::Tools, tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>)> {
        let tools = crate::bridge::Tools::new(dir, dir);
        tools.mcp_actions.attach(ui::Ui::handle(), Arc::new(|_, _, _| Ok(())), Arc::new(|_, _| Ok(())), Arc::new(|_, _| Ok(())))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        *tools.agent_commands.lock().unwrap() = Some(tx);
        tools.enabled.store(false, Ordering::Release);
        Ok((tools, rx))
    }
    fn notices(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::bridge::Command>) -> Vec<(String, String)> {
        let mut out = vec![];
        while let Ok(command) = rx.try_recv() {
            if let crate::bridge::Command::Notice { id, text, .. } = command { out.push((id, text)); }
        }
        out
    }

    /// Two final replies seconds apart, both shown as text: the pull-down gets the first, then both (newest last),
    /// and the agent is told once, as context (no turn), that the user probably missed the first.
    #[test]
    fn two_replies_seconds_apart_keep_the_first_and_tell_the_agent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools(dir.path())?;
        tools.agent_final_answer(MISTRAL, "stop")?;
        assert!(notices(&mut rx).is_empty(), "nothing missed yet");
        tools.agent_final_answer(WEB, "stop")?;
        assert_eq!(*tools.test_island.lock().unwrap(), [
            ui::UiUpdate::LastReply(MISTRAL.into()),
            ui::UiUpdate::LastReply(format!("{MISTRAL}\n\n{WEB}")),
        ]);
        let told = notices(&mut rx);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.starts_with("unheard-speech:unread:"), "{}", told[0].0);
        assert_eq!(agent::notice::NoticeKind::classify(&told[0].0, &told[0].1), agent::notice::NoticeKind::Context, "context, never a turn");
        assert!(told[0].1.contains("It began: \"I found it: it's the Mistral voice agent demo"), "{}", told[0].1);
        Ok(())
    }

    /// A minute apart, the first had its reading time: the second replaces it in the pull-down, nothing is told.
    #[test]
    fn two_replies_a_minute_apart_replace_and_tell_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (tools, mut rx) = tools(dir.path())?;
        tools.agent_final_answer(MISTRAL, "stop")?;
        // The first was shown a minute ago (the tracker's clock moved back instead of the test waiting).
        let minute = std::time::Duration::from_secs(60);
        let shown_at = std::time::Instant::now().checked_sub(minute).expect("a monotonic clock older than a minute");
        *tools.unread.lock().unwrap() = Default::default();
        tools.unread.lock().unwrap().shown_as_text(MISTRAL, shown_at);
        tools.agent_final_answer(WEB, "stop")?;
        assert_eq!(*tools.test_island.lock().unwrap(), [ui::UiUpdate::LastReply(MISTRAL.into()), ui::UiUpdate::LastReply(WEB.into())]);
        assert!(notices(&mut rx).is_empty());
        Ok(())
    }
}
