//! The menu's orange line under the tiles: only problems the user can act on, each one plain sentence (
//! "spot all of them and fix them" — it read "Embedding index: 8 c… · Listening interrupted"). Everything else a
//! component reports (audio gaps, a voice loading, automatic retries and fallbacks, one-off errors) still goes to
//! the health journal and the island; it is not a menu problem.

/// The sentence the menu shows for `component`'s status `message` (with its journaled `reason`), or `None` when
/// it is not something the user can act on. A component that recovers sends `None` itself, so the line clears.
pub(crate) fn menu_problem(component: &str, message: &str, reason: Option<&str>) -> Option<String> {
    let sentence = match component {
        "capture" => "Heimdall can\u{2019}t open the microphone or app audio. Turn Listening off and on to retry.",
        "listen" if message == TRANSCRIPTION_STOPPED => "Transcription stopped. Turn Listening off and on to restart it.",
        "screen" if message == SEEING_STOPPED => "Seeing couldn\u{2019}t start. Turn Seeing off and on to retry.",
        "decision" if message == DECISIONS_STOPPED => "Heimdall can\u{2019}t decide what is meant for it, so it won\u{2019}t answer. Restart Heimdall.",
        "archive" => return Some(archive_sentence(reason.unwrap_or(message)).into()),
        "settings" => "Your settings file has an error, so Heimdall is using the last good settings.",
        "settings-save" => "Heimdall can\u{2019}t save your settings. Check that the disk has free space.",
        "speech" if message == "Speech unavailable" => "Heimdall can\u{2019}t speak right now, so replies only appear on screen.",
        // The index job words its failures ("Search indexing is failing because …"). Only a full disk or a missing
        // model is the user's to fix; a memory pause, a timeout or one unreadable item is retried by the job itself.
        "embedding-index" if message.starts_with("Search indexing")
            && (message.contains("disk is full") || message.contains("not installed")) => return Some(message.into()),
        _ => return None,
    };
    Some(sentence.into())
}

/// `fail`'s held statuses for a part that could not start or keeps stopping; each clears when the part runs again.
pub(crate) const TRANSCRIPTION_STOPPED: &str = "Transcription unavailable";
pub(crate) const SEEING_STOPPED: &str = "Screen unavailable";
pub(crate) const DECISIONS_STOPPED: &str = "Decisions unavailable";

fn archive_sentence(reason: &str) -> &'static str {
    if reason.contains("GiB free") {
        "The disk is almost full, so Heimdall stopped recording. Free some space and it resumes."
    } else if super::resilience::archive_damaged(reason) {
        "The archive database is damaged, so Heimdall isn\u{2019}t recording."
    } else {
        "Heimdall stopped recording because the archive failed. If it doesn\u{2019}t resume soon, restart Heimdall."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status seen in the 22–23 Oct log, as the menu now shows it.
    #[test]
    fn only_problems_owner_can_act_on_reach_the_menu() {
        for (component, message) in [
            ("drama3", "Jarvis voice loading"),
            ("drama3", "Jarvis unloaded to protect free memory; retrying when memory is available."),
            ("drama3", "Drama 3 unavailable. Using the lighter voice."),
            ("gap:mic:AppleUSBAudioEngine:Shure Inc:Shure MV7i:MV7i#7:2,3:1", "Audio gap recorded"),
            ("gap:process:55657:1792668389690256:29", "Audio gap recorded"),
            ("embedding-index", "Embedding index: 8 chunk(s) awaiting retry: audio chunk 39:other_audio:sound:process:55657 has no captured samples"),
            ("embedding-index", "Embedding index: Query returned no rows"),
            ("embedding-index", "Search indexing is failing because the search model is not responding correctly."),
            ("embedding-index", "Search indexing is failing because some screenshots cannot be read."),
            ("embedding-index", "Search indexing is failing because the archive database returned an error."),
            ("pipeline", "Listening interrupted"),
            ("decision", "Decider recovering: worker exited"),
            ("decision", "Decision failed"),
            ("listen", "Final processing failed: x"),
            ("listen-gpu", "Listening using fallback"),
            ("speakers:a", "Speakers using CPU fallback"),
            ("screen", "Screen paused: display asleep"),
            ("screen", "Screen retry 2 in 4s"),
            ("command-decision", "Command not acted on: x"),
            ("agent-screen", "Screenshot unavailable"),
            ("speech", "Speech replies off"),
            ("call-context", "Call context waiting on unavailable speaker labels; retained in archive"),
        ] {
            assert_eq!(menu_problem(component, message, Some(message)), None, "{component}: {message}");
        }
    }

    #[test]
    fn a_menu_problem_is_one_plain_sentence() {
        let shown = [
            menu_problem("capture", "Listening interrupted", Some("Capture: no device")),
            menu_problem("listen", TRANSCRIPTION_STOPPED, Some("Listen failed: transcription stopped 3 times in 10 minutes")),
            menu_problem("screen", SEEING_STOPPED, None),
            menu_problem("decision", DECISIONS_STOPPED, None),
            menu_problem("archive", "Archive paused", Some("Archive paused: 1.8 GiB free")),
            menu_problem("archive", "Archive paused", Some("Archive stopped: database disk image is malformed")),
            menu_problem("settings", "expected value at line 3", None),
            menu_problem("settings-save", "Settings save failed; retrying: x", None),
            menu_problem("speech", "Speech unavailable", None),
            menu_problem("embedding-index", "Search indexing is failing because the disk is full.", None),
            menu_problem("embedding-index", "Search indexing is failing because the search model is not installed.", None),
        ];
        for line in shown {
            let line = line.expect("an actionable problem is shown");
            assert!(line.ends_with('.') && line.chars().next().unwrap().is_uppercase(), "{line}");
            assert!(!line.contains(':') && !line.contains('·'), "no raw error fragments: {line}");
            assert!(line.chars().count() <= 110, "two lines at the menu's narrowest width: {line}");
        }
    }
}
