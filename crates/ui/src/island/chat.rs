//! The conversation the pulled-down notch shows ("literally a chat conversation between you and
//! me, and we can go up and down … not the actual context, just the conversation between you and me"). The app builds
//! it from what Heimdall already stores (`crates/app/src/chat_history.rs`) and sends it whole (`UiUpdate::Chat`);
//! the island places the last turn it shows live (reply and notes) among it (`split`).
//!
//! Only the user's own words to Heimdall, Heimdall's replies and spoken lines, and compact actions belong here: never
//! other speakers, app audio or the agent's context.

/// Whether the user heard a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Heard {
    /// Said whole, or shown without speech.
    Whole,
    /// The user talked over it: its speech was cut short.
    Cut,
    /// Never reached him (skipped while he talked, or dropped unheard).
    Never,
}

/// What a compact action row stands for: its icon and colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// A task started (a worker thread, a background job).
    Started,
    /// A task finished.
    Finished,
    /// A task failed or was cancelled.
    Failed,
    /// A question card, with the option the user chose.
    Question,
}

/// The drawing a notice carried, for its row's icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteShape {
    Text,
    Table,
    Svg,
    Image,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatKind {
    /// The user's words delivered to Heimdall (his microphone's addressed turn, or a typed message).
    Owner(String),
    /// A reply or a spoken line, as written.
    Heimdall { text: String, heard: Heard },
    /// A task or question, one short line; `answer` is the option the user chose on a question card.
    Action { kind: ActionKind, text: String, answer: Option<String> },
    /// A notice with a drawing (`island_notice` with a detail): a tap pulls it out as a floating island.
    Note { id: String, title: String, shape: NoteShape },
}

/// One row of the conversation, `at_ms` Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatEntry {
    pub at_ms: u64,
    pub kind: ChatKind,
}

/// The rows around the live turn: `before` it (earlier turns and the words that asked for it) and `after` it (what
/// happened since, before the next reply).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Around {
    pub before: Vec<ChatKind>,
    pub after: Vec<ChatKind>,
}

/// Whitespace-insensitive text for matching the live reply with its stored row.
pub fn same_text(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// Whether the live reply `reply` holds the stored row's words `row` whole (whitespace-insensitive): the pull-down can
/// show several replies at once (unread ones first, newest last) or the turn's spoken lines joined.
pub fn holds(reply: &str, row: &str) -> bool {
    Held::new(reply).holds(row)
}

/// A live reply's words, padded for whole-word containment (made once per split).
struct Held(String);

impl Held {
    fn new(reply: &str) -> Held {
        Held(format!(" {} ", reply.split_whitespace().collect::<Vec<_>>().join(" ")))
    }
    fn holds(&self, row: &str) -> bool {
        let row: Vec<&str> = row.split_whitespace().collect();
        !row.is_empty() && self.0.contains(&format!(" {} ", row.join(" ")))
    }
}

/// How many of the user's turns back the live reply is looked for (he may have spoken again since it came).
const TURNS_BACK: usize = 3;

/// Places the live turn (its reply `reply`, empty when none, and its notes `notes` by id) in `history`. Its reply is
/// looked for among Heimdall's rows after one of the user's last few turns (the newest such row it holds splits); a turn of
/// notes alone splits at its first note. Only the live turn's own rows are left out: the replies it holds in that one
/// stretch of the conversation (an earlier "Done." elsewhere in the day stays) and its notes by id. Not found (the
/// record is still on its way), everything is before.
pub fn split(history: &[ChatEntry], reply: &str, notes: &[String]) -> Around {
    let held = Held::new(reply);
    let note_of = |k: &ChatKind| matches!(k, ChatKind::Note { id, .. } if notes.iter().any(|n| n == id));
    let held_row = |k: &ChatKind| matches!(k, ChatKind::Heimdall { text, .. } if held.holds(text));
    // (split index, the stretch whose held replies are the turn's own)
    let mut found: Option<(usize, usize, usize)> = None;
    if reply.trim().is_empty() {
        found = history.iter().position(|e| note_of(&e.kind)).map(|i| (i, i, i));
    } else {
        let mut bounds: Vec<usize> = history.iter().enumerate().filter(|(_, e)| matches!(e.kind, ChatKind::Owner(_))).map(|(i, _)| i + 1).collect();
        bounds.insert(0, 0);
        let mut end = history.len();
        for &start in bounds.iter().rev().take(TURNS_BACK + 1) {
            if let Some(i) = history[start..end].iter().rposition(|e| held_row(&e.kind)) {
                found = Some((start + i, start, end));
                break;
            }
            end = start.saturating_sub(1);
        }
    }
    let keep = |range: std::ops::Range<usize>, own: &std::ops::Range<usize>| {
        history[range.clone()].iter().enumerate().filter(|(k, e)| !note_of(&e.kind) && !(own.contains(&(range.start + k)) && held_row(&e.kind))).map(|(_, e)| e.kind.clone()).collect::<Vec<_>>()
    };
    match found {
        Some((i, s, e)) => Around { before: keep(0..i, &(s..e)), after: keep(i + 1..history.len(), &(s..e)) },
        None => Around { before: keep(0..history.len(), &(0..0)), after: vec![] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(at_ms: u64, kind: ChatKind) -> ChatEntry {
        ChatEntry { at_ms, kind }
    }
    fn owner(s: &str) -> ChatKind {
        ChatKind::Owner(s.into())
    }
    fn him(s: &str) -> ChatKind {
        ChatKind::Heimdall { text: s.into(), heard: Heard::Whole }
    }
    fn note(id: &str) -> ChatKind {
        ChatKind::Note { id: id.into(), title: id.into(), shape: NoteShape::Table }
    }

    #[test]
    fn the_live_reply_splits_the_conversation_and_shows_once() {
        let h = vec![e(1, owner("How much RAM?")), e(2, him("About 60 GB.")), e(3, owner("And swap?")), e(4, note("swap")), e(5, him("None.\n\nAll good.")), e(6, owner("Thanks"))];
        let a = split(&h, "None. All good.", &["swap".into()]);
        assert_eq!(a.before, vec![owner("How much RAM?"), him("About 60 GB."), owner("And swap?")], "the reply's own note is the live turn's");
        assert_eq!(a.after, vec![owner("Thanks")]);
    }

    #[test]
    fn a_reply_said_twice_splits_at_its_last_row_and_a_missing_record_keeps_everything_before() {
        let h = vec![e(1, owner("Status?")), e(2, him("Done.")), e(3, owner("Status again?")), e(4, him("Done."))];
        let a = split(&h, "Done.", &[]);
        assert_eq!(a.before, vec![owner("Status?"), him("Done."), owner("Status again?")], "the same words in an earlier turn stay: only the live turn's own row goes");
        assert!(a.after.is_empty());
        let a = split(&h, "Something new", &[]);
        assert_eq!(a.before.len(), 4);
        assert!(a.after.is_empty());
    }

    #[test]
    fn a_pull_down_holding_several_replies_hides_each_and_splits_at_the_newest() {
        let h = vec![e(1, owner("Two things")), e(2, him("Mistral is cheaper.")), e(3, him("The web page is up.")), e(4, owner("Ok"))];
        let a = split(&h, "Mistral is cheaper.\n\nThe web page is up.", &[]);
        assert_eq!(a.before, vec![owner("Two things")]);
        assert_eq!(a.after, vec![owner("Ok")]);
        assert!(!holds("Done and dusted.", "Done."), "whole words only");
        assert!(!holds("Undone.", "done."));
        assert!(holds("It is done.  Really.", "done. Really."));
    }

    #[test]
    fn a_live_reply_whose_record_is_late_never_splits_at_an_old_twin() {
        // "Done." two turns back; the live "Done. The build passed." is not in the record yet.
        let h = vec![e(1, owner("a")), e(2, him("Done.")), e(3, owner("b")), e(4, him("Sure.")), e(5, owner("c")), e(6, owner("d")), e(7, owner("e")), e(8, owner("f"))];
        let a = split(&h, "Done. The build passed.", &[]);
        assert_eq!(a.before.len(), h.len(), "too far back: not the live turn; nothing hidden, nothing after");
        assert!(a.after.is_empty());
    }

    #[test]
    fn a_turn_of_notes_only_splits_at_its_first_note() {
        let h = vec![e(1, owner("Show the table")), e(2, note("t1")), e(3, note("t2")), e(4, owner("Later"))];
        let a = split(&h, "", &["t1".into(), "t2".into()]);
        assert_eq!(a.before, vec![owner("Show the table")]);
        assert_eq!(a.after, vec![owner("Later")]);
    }
}
