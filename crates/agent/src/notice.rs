//! What a non-the user message to the agent is allowed to do, and the text of every start.
//!
//! A notice (T3, worker, question answer, fast-read result) never steers a turn that serves the user: it waits for the
//! turn to settle, coalesces with the others waiting, and at most one notice-only turn runs per 30 seconds
//! (lab/harness/EXPECTED.md E1.6–E1.10, E1.18, E2.2–E2.4). Context-only kinds are appended without a turn.
//! Every start appends the catch-up and the restart message before any wake (E4.1).

/// How long a start looks back, how old pending context may be before a wake drops it, and how old a queued notice
/// may be before it becomes context instead of a turn ("we append the last 30 minutes").
pub const CONTEXT_WINDOW_MS: u64 = 30 * 60_000;
/// At most one notice-only turn per this interval; notices inside it coalesce (E1.18).
pub const NOTICE_GAP_MS: u64 = 30_000;
/// A failed delivery is resent once only while the failure is younger than this, in the same process (E1.14).
pub const RESEND_WINDOW_MS: u64 = 2 * 60_000;
/// The user's wake that failed at the provider for a passing reason (`transient_failure`: a timeout, a dropped connection,
/// an overload or a server error) is resent after each of these delays in turn, then given up (bug notes 26 Oct
/// 00:52:29 and 00:54:39: six timeouts in a row, one resend, then nothing until the user spoke again). Each attempt is
/// already Pi's own three tries over about 40 s, so the whole series ends within about four minutes; every delay is
/// shorter than `RESEND_WINDOW_MS`, which counts from the latest failure.
pub const RETRY_DELAYS_MS: [u64; 3] = [5_000, 15_000, 45_000];
/// A provider failure a later attempt can get past: the request timed out or never connected, the provider was
/// overloaded or failed on its side. A refusal (`provider_rejection`) and anything unrecognised are not transient.
pub fn transient_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    let status = lower.trim_start().get(..3).and_then(|code| code.parse::<u16>().ok());
    status.is_some_and(|s| (500..600).contains(&s))
        || ["timed out", "timeout", "connection error", "connection reset", "socket hang up", "fetch failed", "network",
            "econnreset", "econnrefused", "etimedout", "enotfound", "eai_again", "overloaded", "api_error",
            "internal server error", "bad gateway", "service unavailable", "gateway timeout"]
            .iter().any(|p| lower.contains(p))
}
/// A plan usage limit or quota is spent: ChatGPT's "reached their Subscription Sharing usage limit"
/// (`subscription_sharing_usage_limit_exceeded`), Codex's `usage_limit_reached`, OpenAI's `insufficient_quota`. It lasts
/// until the window resets, hours away, so a resend fails the same way (26 Oct 23:33–23:35: every wake failed twice).
/// A per-minute "rate limit" is not one.
pub fn quota_exhausted(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    ["usage limit", "usage_limit", "insufficient_quota", "quota exceeded", "quota_exceeded"].iter().any(|p| lower.contains(p))
}
/// Outbox id prefixes of emergency mode (`NoticeKind::classify`).
pub const EMERGENCY_COVER: &str = "emergency-cover:";
/// The hand-back notice's key is its delivery id as well (`bridge/supervisor.rs`), and Pi takes only letters, digits,
/// `-` and `_` there: a colon made the 27 Oct 03:12 hand-back fail with "invalid delivery id".
pub const EMERGENCY_BACK: &str = "emergency-back-";
/// The installed build's earlier hand-back prefix, for outbox rows queued before the change.
pub const EMERGENCY_BACK_OLD: &str = "emergency-back:";
/// Input that arrived while emergency mode was ending: nobody answered it, so it wakes the agent and is never held.
/// Id-safe like `EMERGENCY_BACK` (night audit: the outbox hashes it today, but nothing should depend on that).
pub const EMERGENCY_LATE: &str = "emergency-late-";
/// The earlier prefix, still classified for outbox rows queued before the change.
const EMERGENCY_LATE_OLD: &str = "emergency-late:";
/// A notice whose text repeats one already delivered within this window is dropped (E6.4).
pub const REPEAT_WINDOW_MS: u64 = 24 * 60 * 60_000;
/// The user's request (a voice wake or typed text) queued at most this long before a start — while a reload waited for
/// its turn, Pi compacted, or the process restarted — is still a request: it goes out after the start context and
/// gets its answer (bug note 20 Oct 22:12:57). Older queued requests are history (E4.4).
pub const START_RESUME_MS: u64 = 5 * 60_000;
/// The same labelled words within this interval are one utterance heard on two microphones (bug note 21 Oct 00:10:14:
/// AirPods and the Shure each turned "Okay, I have both enabled right now…" into a wake, 173 ms apart). Every
/// microphone is labelled `Mic`, so the user repeating a short line this fast counts too; its words already went out.
pub const HEARD_TWICE_MS: u64 = 3_000;
/// About 32,000 tokens: after a compaction the summary, the kept tail and the catch-up still fit the request budget.
pub const CATCHUP_BYTES: usize = 128 * 1024;

/// The id of the first-run greeting the app sends once per profile when Setup finishes.
pub const FIRST_RUN: &str = "first-run-greeting";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// The user clicked an answer to Heimdall's own `ask` (E1.6): a wake, and it counts as the user addressing Heimdall.
    Answer,
    /// A T3 thread asks a question or waits for an approval (E1.7, E1.8): a wake, held until the user has spoken after a
    /// start.
    Question,
    /// An owned worker finished, failed or is stuck (E1.7): a wake; before the user has spoken after a start, context.
    Event,
    /// The result of a task Heimdall started or a run it sent a message into (`terminal:` ids from crates/t3): a wake
    /// also right after a start while the user has not spoken, and one still queued at a start goes out after it, once
    /// (after a rebuild and restart the agent keeps receiving its tasks' completion notices; at
    /// 04:00:52 the night pass's result became context 29 minutes after the install's start and woke nobody).
    Result,
    /// Fast-read result, worker progress, unheard speech, connection state (E1.10): context only, never a turn.
    Context,
    /// A scheduled wake (`agent::wakes`, `selfwake:` ids; night 21 Oct): the agent's own note to itself, or a local
    /// agent's message, due now. A wake also right after a start (the user has not spoken yet) and when it waited out the
    /// context window: it was asked for exactly this moment.
    Scheduled,
}
impl NoticeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Question => "question",
            Self::Event => "event",
            Self::Result => "result",
            Self::Context => "context",
            Self::Scheduled => "scheduled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "answer" => Self::Answer,
            "question" => Self::Question,
            "event" => Self::Event,
            "result" => Self::Result,
            "context" => Self::Context,
            "scheduled" => Self::Scheduled,
            _ => return None,
        })
    }
    pub fn wakes(self) -> bool {
        !matches!(self, Self::Context)
    }
    /// Order inside one coalesced message after settlement: answers, then T3 and worker notices (E2.3).
    pub fn rank(self) -> u8 {
        match self {
            Self::Answer => 0,
            Self::Question => 1,
            Self::Event => 1,
            Self::Result => 1,
            Self::Scheduled => 1,
            Self::Context => 2,
        }
    }
    /// The kind of an outbox delivery, from its deterministic id (E6.3) and text. Ids: `ask-…` (answer click),
    /// `fastread-…`, `terminal:` (Heimdall's tasks and watched runs; `stuck:` is retired), `question:/approval:` (their requests),
    /// `t3:{thread}:{run}:{kind}:…` and `t3:batch:[attention:]{digest}` (T3 policy), `t3-connect-…`, `unheard-speech:…`; the legacy observer's random
    /// `t3-activity:` lines are judged by their words.
    pub fn classify(id: &str, text: &str) -> Self {
        let lower = text.to_ascii_lowercase();
        if id.starts_with(crate::wakes::OUTBOX_PREFIX) {
            Self::Scheduled
        } else if id.starts_with("ask-") || (text.starts_with("Question ") && text.contains(": User clicked ")) {
            Self::Answer
        // The user's pick on an island suggestion card (crates/app/src/suggester.rs): he chose it, so it wakes like an answer.
        } else if id.starts_with("suggest-pick:") {
            Self::Answer
        // Floating islands (night 1022): the user's approvals, buttons and controls wake like an `ask` click; his ticks,
        // opens, closes and thread answers ride along as context (crates/app/src/islands/responses.rs).
        } else if id.starts_with("islands-answer:") {
            Self::Answer
        } else if id.starts_with("islands-context:") {
            Self::Context
        // The plan refresher's diff (night 1022): context only. A wake would start a turn, grow the session and make
        // the next pass see something new: a loop every 5 minutes (crates/app/src/plan_refresher.rs).
        } else if id.starts_with("plan-refresh-") {
            Self::Context
        // Emergency mode (docs/emergency.md): what the cover agent did is the record, context only; the hand-back's
        // "while you were down" wakes the agent like a scheduled wake, also right after a start.
        } else if id.starts_with(EMERGENCY_COVER) {
            Self::Context
        } else if id.starts_with(EMERGENCY_BACK) || id.starts_with(EMERGENCY_BACK_OLD) || id.starts_with(EMERGENCY_LATE) || id.starts_with(EMERGENCY_LATE_OLD) {
            Self::Scheduled
        // The one greeting after a profile's first Setup (app setup_wizard.rs): a wake although nobody has spoken yet.
        } else if id.starts_with(FIRST_RUN) {
            Self::Scheduled
        } else if id.starts_with("fastread-")
            || id.starts_with("unheard-speech")
            // An instant acknowledgement that played after its wake had gone (app pipeline/ack.rs `late_ack`).
            || id.starts_with("ack-said:")
            || id.starts_with("t3-connect")
            || id.starts_with("t3-offline")
            || text.starts_with("Worker monitoring")
            || text.starts_with("Connect T3")
            || text.starts_with("Fast read")
        {
            Self::Context
        } else if id.starts_with("terminal:") {
            Self::Result
        } else if id.starts_with("question:") || id.starts_with("approval:") {
            Self::Question
        } else if let Some(rest) = id.strip_prefix("t3:") {
            if rest.split(':').any(|part| matches!(part, "attention" | "question" | "approval" | "waiting")) {
                Self::Question
            } else {
                Self::Event
            }
        } else if id.starts_with("t3-activity:") {
            if lower.contains("waiting for") || lower.contains("question") || lower.contains("approv") {
                Self::Question
            } else {
                Self::Event
            }
        } else {
            Self::Event
        }
    }
}

/// The kind a queued delivery was stored with. A task's result queued by a build from before `Result` existed was
/// stored as an `event`; its words name it.
pub fn stored_kind(wire: &serde_json::Value) -> Option<NoticeKind> {
    let kind = wire["notice"].as_str().and_then(NoticeKind::parse)?;
    let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("");
    Some(if kind == NoticeKind::Event && text.starts_with("[Heimdall notice: terminal:") { NoticeKind::Result } else { kind })
}

/// `[17:52:38 Mic · User] words`: each labelled line of a transcript row gets its clock time.
pub fn timed(labelled: &str, at_ms: u64) -> String {
    stamped(labelled, Some(at_ms))
}
/// `timed`, or `(earlier)` in place of a time for a row whose time is unknown (a legacy queued delivery).
pub fn stamped(labelled: &str, at_ms: Option<u64>) -> String {
    let stamp = at_ms.map(crate::clock::local_hms).unwrap_or_else(|| "(earlier)".into());
    labelled
        .split('\n')
        .map(|line| match line.strip_prefix('[') {
            Some(rest) => format!("[{stamp} {rest}"),
            None => line.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last thing every start appends (E4.1). Nothing above it is a request.
pub fn restart_message(at_ms: u64) -> String {
    format!(
        "Heimdall, you have just been restarted (or turned on) at {}. Everything above is historical context. Don't do anything unless User talks to you.",
        crate::clock::local_hm(at_ms)
    )
}

/// One history line for the catch-up: a transcript row, a typed message or a notice the conversation never received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchupLine {
    pub id: String,
    /// When it was heard or queued; `None` when the row never recorded a time (it reads `(earlier)`).
    pub at_ms: Option<u64>,
    /// Labelled text without its time (`[Mic · User] words`).
    pub text: String,
    /// A spool observation (recorded as supplied once added) rather than a typed message or a notice.
    pub observation: bool,
}

/// What one start appends before any wake: the catch-up lines (oldest first, newest kept within `CATCHUP_BYTES`),
/// an optional reason line (a reload's), and the restart message, last.
#[derive(Clone, Debug, Default)]
pub struct StartContext {
    pub at_ms: u64,
    pub reason: Option<String>,
    pub lines: Vec<CatchupLine>,
    /// A compaction ran first because the catch-up would have passed the threshold (E4.2).
    pub compacted: bool,
    /// The process this context failed on once; a second failure drops the catch-up with a log line.
    pub failed_on: Option<u32>,
    /// Where the catch-up begins (`Bridge::catchup_since`): the header names it.
    pub since_ms: Option<u64>,
}
impl StartContext {
    pub fn new(at_ms: u64, reason: Option<String>) -> Self {
        Self { at_ms, reason, ..Default::default() }
    }
    /// Adds lines, keeps them in time order without duplicates and bounds them, newest kept.
    pub fn add_lines(&mut self, lines: impl IntoIterator<Item = CatchupLine>) {
        for line in lines {
            if !self.lines.iter().any(|l| l.id == line.id) {
                self.lines.push(line);
            }
        }
        self.lines.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.id.cmp(&b.id)));
        let mut bytes = 0;
        let keep = self.lines.iter().rev().take_while(|l| {
            bytes += l.text.len() + 12;
            bytes <= CATCHUP_BYTES
        }).count();
        let drop = self.lines.len() - keep;
        self.lines.drain(..drop);
    }
    pub fn text(&self) -> String {
        let mut text = String::new();
        if !self.lines.is_empty() {
            // A spool line the model never received may be older than where the archived part begins: the header
            // names the earliest line, so it never claims less than the lines below show.
            let since = self.since_ms.map(|at| self.lines.iter().filter_map(|l| l.at_ms).fold(at, u64::min))
                .map(|at| format!("since {}", crate::clock::local_hms(at))).unwrap_or_else(|| "before this start".into());
            text.push_str(&format!("Heimdall catch-up: what was said {since} that the conversation did not receive (history only; nothing here is a request).\n"));
            for line in &self.lines {
                text.push_str(&stamped(&line.text, line.at_ms));
                text.push('\n');
            }
            text.push('\n');
        }
        if let Some(reason) = self.reason.as_deref().filter(|r| !r.trim().is_empty()) {
            text.push_str(reason.trim());
            text.push('\n');
        }
        text.push_str(&restart_message(self.at_ms));
        text
    }
}

/// A provider refusal that a resend cannot fix (400 invalid request other than a thinking signature, which
/// `provider-retry.mjs` resends): the plain-language reason for the island (E10.2), or `None`. `repaired`: the run's
/// own history repair (`runtime/history-repair.mjs`) acted before this refusal.
pub fn provider_rejection(error: &str, repaired: bool) -> Option<String> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("signature") && lower.contains("thinking") {
        return None;
    }
    // A spent usage limit is not resent: the next request fails the same way until the window resets. Heimdall says
    // which limit and when it resets once (runtime/quota.mjs, the app's `quota:` line); the island names it here.
    if quota_exhausted(error) {
        return Some("Agent couldn't answer: the plan's usage limit is used up; your words go with the next request".into());
    }
    // provider-retry.mjs aborts a request with no response headers in 120 s, longer for a prompt above 200k tokens
    // (E10.7): resending at once would wait as long again, so the words ride the next wake instead.
    if lower.contains("did not answer within") {
        return Some("Agent couldn't answer: the model did not start answering in time; your words go with the next request".into());
    }
    let rejected = lower.contains("invalid_request_error")
        || (lower.contains("400") && (lower.contains("bad request") || lower.contains("invalid request")));
    if !rejected {
        return None;
    }
    // Out of credits: a resend fails the same way, and only the user can add credit (E10.6).
    if lower.contains("credit balance is too low") {
        return Some("Agent couldn't answer: Anthropic credits used up; your words go with the next request".into());
    }
    let what = if lower.contains("image") && (lower.contains("exceed") || lower.contains("too large") || lower.contains("too big") || lower.contains("dimension")) {
        "the screenshot was too large for the model"
    } else if lower.contains("prompt is too long") || lower.contains("context length") || lower.contains("too many tokens") {
        "the conversation was too long for the model"
    } else if lower.contains("image") {
        "the model could not read the screenshot"
    } else {
        "the model refused the request as invalid"
    };
    // A refusal naming a block of the history: the conversation itself is broken, so no resend is promised (20 Oct
    // 19:22–19:24 the island promised three times that the words would go with the next request). "After repair" only
    // when the run's repair acted: a refusal it did not recognise, or one without the repair, gets no such claim.
    if names_history_block(error) {
        return Some(if repaired {
            format!("Agent couldn't answer: the model still refuses this conversation after repair ({what})")
        } else {
            format!("Agent couldn't answer: the model refuses this conversation ({what})")
        });
    }
    Some(format!("Agent couldn't answer: {what}; your words go with the next request"))
}
/// The error names a block of the conversation history: Anthropic `messages.N.…`, OpenAI `messages[N]…`.
pub fn names_history_block(error: &str) -> bool {
    ["messages.", "messages["].iter().any(|p| error.match_indices(p).any(|(at, _)| error[at + p.len()..].starts_with(|c: char| c.is_ascii_digit())))
}

#[cfg(test)]
mod tests {
    /// 27 Oct 03:12: the hand-back's key is sent to Pi as the delivery id, which allows only [A-Za-z0-9_-].
    #[test]
    fn the_hand_back_key_is_a_valid_delivery_id() {
        // Late input's key carries the relayed input's id (a wake id: letters, digits and dashes).
        for key in [format!("{}{}", EMERGENCY_BACK, 1_793_067_134_195u64), format!("{}{}", EMERGENCY_LATE, "w-1793067134195")] {
            assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "{key}");
        }
        assert_eq!(NoticeKind::classify("emergency-late:w9", "x"), NoticeKind::classify(&format!("{EMERGENCY_LATE}w9"), "x"));
        assert_eq!(NoticeKind::classify("emergency-back:1793067134195", "x"), NoticeKind::classify(&format!("{EMERGENCY_BACK}1793067134195"), "x"), "a hand-back queued by the installed build");
    }

    use super::*;
    #[test]
    fn passing_provider_failures_are_transient_refusals_are_not() {
        for e in ["Request timed out.", "Connection error.", "529 overloaded_error", "500 {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}",
                  "503 Service Unavailable", "fetch failed", "read ECONNRESET"] {
            assert!(transient_failure(e), "{e}");
        }
        for e in ["400 invalid_request_error: messages.3.content.0: Invalid `signature` in `thinking` block", "401 authentication_error",
                  "Anthropic: key rejected", "something odd"] {
            assert!(!transient_failure(e), "{e}");
        }
        assert!(RETRY_DELAYS_MS.windows(2).all(|w| w[1] > w[0]), "the delays grow");
        assert!(RETRY_DELAYS_MS.iter().all(|d| *d < RESEND_WINDOW_MS));
    }
    #[test]
    fn every_outbox_id_has_its_kind() {
        let k = NoticeKind::classify;
        assert_eq!(k("ask-1b2c", "Question ask-1b2c: User clicked Yes. Question: Install?"), NoticeKind::Answer);
        // The user's pick on an island suggestion card (crates/app/src/suggester.rs): he chose it, so it wakes like an answer.
        assert_eq!(k("suggest-pick:5f0a", "[Heimdall suggestion, picked by User] Topic: CAD file."), NoticeKind::Answer);
        assert!(NoticeKind::Answer.wakes());
        assert_eq!(k("fastread-9", "Fast read: read at 390 wpm."), NoticeKind::Context);
        assert_eq!(k("islands-answer:1f2e", "User chose Yes on the approval 'Delete old caches'."), NoticeKind::Answer);
        assert_eq!(k("islands-context:9a8b", "User ticked 'Arsenal: push' in To do."), NoticeKind::Context);
        assert_eq!(k("plan-refresh-1792630210300", "Plan: added \u{201c}Arsenal: push branch\u{201d}."), NoticeKind::Context);
        assert!(!NoticeKind::Context.wakes(), "the plan refresher's notice never wakes the agent");
        assert_eq!(k("terminal:t:r:completed", "Worker t run r completed."), NoticeKind::Result);
        assert_eq!(k("terminal:t:r:completed:again", "Task t completed."), NoticeKind::Result);
        assert!(NoticeKind::Result.wakes());
        assert_eq!(NoticeKind::parse(NoticeKind::Result.as_str()), Some(NoticeKind::Result));
        assert_eq!(k("stuck:t:r:a", "Worker t looks stuck."), NoticeKind::Event);
        assert_eq!(k("question:t:q", "Worker t asks: which branch?"), NoticeKind::Question);
        assert_eq!(k("approval:t:idle", "Worker t waits for approval."), NoticeKind::Question);
        assert_eq!(k("t3:mcp:9209:run-640:attention:q1", "Mobius asks …"), NoticeKind::Question);
        assert_eq!(k("t3:mcp:9209:run-640:finished", "Mobius finished"), NoticeKind::Event);
        // crates/t3's observer delivers coalesced batches; a batch carrying a question or approval says so.
        assert_eq!(k("t3:mcp:9209:run-640:done", "T3 update:\n- 'Mine' (yours) completed: ok"), NoticeKind::Event);
        assert_eq!(k("t3:batch:5f2a9c", "T3 update:\n- 'Mine' (yours) completed: ok"), NoticeKind::Event);
        assert_eq!(k("t3:batch:attention:5f2a9c", "T3 update:\n- 'Pick region' asks: which?"), NoticeKind::Question);
        assert_eq!(k("t3:t-approve:p1:attention:approval", "waits for an approval"), NoticeKind::Question);
        assert_eq!(k("t3-activity:1234", "T3 update: 'Mobius' completed: No, it's not me"), NoticeKind::Event);
        assert_eq!(k("t3-activity:1235", "T3: 'Mobius' is waiting for your answer"), NoticeKind::Question);
        assert_eq!(k("t3-connect-401:env", "Connect T3 · click"), NoticeKind::Context);
        assert_eq!(k("w", "Worker monitoring paused"), NoticeKind::Context);
        assert_eq!(k("unheard-speech:1", "Done."), NoticeKind::Context);
        // Scheduled wakes (agent::wakes): their own kind, a wake, whatever the note says.
        assert_eq!(k("selfwake:wake-3f2a1c", "Scheduled wake wake-3f2a1c: … Your note: ask User a question"), NoticeKind::Scheduled);
        assert!(NoticeKind::Scheduled.wakes());
        assert_eq!(k(FIRST_RUN, "This is your first run on the user's Mac"), NoticeKind::Scheduled);
        assert_eq!(NoticeKind::parse(NoticeKind::Scheduled.as_str()), Some(NoticeKind::Scheduled));
    }
    #[test]
    fn lines_carry_clock_times_and_the_restart_message_comes_last() {
        let at = 1_760_801_558_000; // any instant; the label is local time
        assert_eq!(timed("[Mic · User] hi\n[Discord · Dave] yo", at), format!("[{0} Mic · User] hi\n[{0} Discord · Dave] yo", crate::clock::local_hms(at)));
        let mut ctx = StartContext::new(at, Some("Reloaded to pick up new tools".into()));
        ctx.add_lines([
            CatchupLine { id: "b".into(), at_ms: Some(at - 60_000), text: "[Mic · User] second".into(), observation: true },
            CatchupLine { id: "a".into(), at_ms: Some(at - 120_000), text: "[Mic · User] first".into(), observation: true },
            CatchupLine { id: "a".into(), at_ms: Some(at - 120_000), text: "[Mic · User] first".into(), observation: true },
        ]);
        let text = ctx.text();
        assert!(text.find("first").unwrap() < text.find("second").unwrap(), "{text}");
        assert_eq!(text.matches("first").count(), 1);
        let reason = text.find("Reloaded to pick up new tools").unwrap();
        assert!(text.find("second").unwrap() < reason);
        assert!(text.ends_with(&restart_message(at)), "{text}");
        assert!(restart_message(at).contains("Don't do anything unless User talks to you"));
        assert!(StartContext::new(at, None).text().starts_with("Heimdall, you have just been restarted"));
    }
    #[test]
    fn catch_up_keeps_the_newest_lines_within_its_bound() {
        let mut ctx = StartContext::new(10_000_000, None);
        ctx.add_lines((0..4000).map(|n| CatchupLine { id: format!("{n:05}"), at_ms: Some(n), text: format!("[Mic · User] {}", "x".repeat(60)), observation: true }));
        assert!(ctx.lines.len() < 4000 && ctx.lines.len() > 1000);
        assert_eq!(ctx.lines.last().unwrap().id, "03999");
        assert!(ctx.text().len() <= CATCHUP_BYTES + 4096);
    }
    #[test]
    fn only_unfixable_400s_are_rejections_and_they_read_as_words() {
        assert_eq!(provider_rejection(r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"image exceeds 5 MB maximum"}}"#, false).as_deref(),
            Some("Agent couldn't answer: the screenshot was too large for the model; your words go with the next request"));
        // Naming a block of the history: the run's own repair gave up, the conversation is broken; nothing promised.
        let refused = r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"messages.118.content.0.tool_result.content.0.image.source.base64: The image was specified using the image/png media type, but the image appears to be a image/jpeg image"}}"#;
        assert_eq!(provider_rejection(refused, true).as_deref(),
            Some("Agent couldn't answer: the model still refuses this conversation after repair (the model could not read the screenshot)"));
        // No repair ran in that run (night review 21 Oct, minor): no claim that one did.
        assert_eq!(provider_rejection(refused, false).as_deref(),
            Some("Agent couldn't answer: the model refuses this conversation (the model could not read the screenshot)"));
        assert!(names_history_block("Invalid 'messages[12].content[1].image_url'") && !names_history_block("messages.: none") && !names_history_block("too many messages"));
        assert!(provider_rejection(r#"400 {"error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens"}}"#, false).unwrap().contains("conversation was too long"));
        assert!(provider_rejection(r#"400 invalid_request_error: messages: Invalid `signature` in `thinking` block"#, false).is_none());
        assert!(provider_rejection("503 Service Unavailable", false).is_none());
        assert!(provider_rejection("429 rate limit", false).is_none());
        // A spent plan limit (26 Oct 23:33, Pi's exact text for ChatGPT's SSE error) is refused, never resent.
        for e in ["The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.",
                  "OpenAI API error (429): {\"code\":\"subscription_sharing_usage_limit_exceeded\"}", "429 usage_limit_reached", "429 insufficient_quota"] {
            assert!(quota_exhausted(e), "{e}");
            assert!(!transient_failure(e), "{e}");
            assert!(provider_rejection(e, false).unwrap().contains("usage limit is used up"), "{e}");
        }
        assert!(!quota_exhausted("429 Rate limit reached for requests") && !quota_exhausted("529 overloaded_error"));
        assert!(provider_rejection("Stopped by User", false).is_none());
        assert!(provider_rejection("Anthropic did not answer within 120 seconds", false).unwrap().contains("did not start answering in time"));
    }
    #[test]
    fn exhausted_credits_read_as_credits_used_up_and_are_not_resent() {
        let error = r#"400 {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#;
        let reason = provider_rejection(error, false).expect("a rejection, so the turn is not resent");
        assert!(reason.contains("Anthropic credits used up"), "{reason}");
        assert!(!reason.contains("invalid"), "{reason}");
    }
    #[test]
    fn a_catch_up_line_without_a_time_reads_earlier_not_a_clock() {
        let at = 1_760_801_558_000;
        assert_eq!(stamped("[Notice] check the build", None), "[(earlier) Notice] check the build");
        assert_eq!(stamped("[Notice] check the build", Some(at)), timed("[Notice] check the build", at));
        let mut ctx = StartContext::new(at, None);
        ctx.add_lines([CatchupLine { id: "old".into(), at_ms: None, text: "[Typed · User] old words".into(), observation: false }]);
        let text = ctx.text();
        assert!(text.contains("[(earlier) Typed · User] old words"), "{text}");
    }
}
