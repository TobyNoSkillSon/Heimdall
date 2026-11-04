//! Where a voice request's time goes: one record per request Heimdall answers, every stage stamped on the host clock
//! (`capture::host_now_ns`, the clock capture stamps audio with), printed as one `latency …` line in the app log once
//! the request settles. `lab/latency/map.py` turns those lines into the per-stage table of `lab/latency/MAP.md`.
//!
//! Line format (`v=1`): `latency v=1 id=<utterance> trigger=<name|follow_up|d1|-> forced=<0|1> calls=<model calls>
//! busy=<0|1> first_tool=<name|-> settled=<done|superseded> <stage>=<ms|-> … total=<ms>`. Every stage is
//! milliseconds after the speech end (the utterance's last voiced audio, so 0); `-` means it did not happen. `busy=1`:
//! the request was handed over while the agent was already working or had to queue (its wait shows in `request`). `total`: first audio, else the turn's end.
//!
//! Stages in order: `endpoint` the engine decided the pause was long enough, `smartturn` Smart Turn answered, `asr` the
//! final Parakeet decode finished, `final` the conductor received the final, `admit` the conductor admitted it for the
//! agent (after any spoken-command check), `decide` the d1 decision, `deliver` the wake was handed to Pi, `request` Pi
//! started the model call (`turn_start`), `stream` the provider's response began (`message_start`), `think`/`text`/
//! `toolcall` the first thinking, text and tool-call tokens, `tool` the first tool started, `speak` the first speech
//! was queued, `audio` Heimdall's answer became audible (the first sample at the speaker; never an acknowledgement),
//! `done` the turn completed.
//!
//! After `total` (26 Oct 22:26, the user: end of his speech to the first audible actual answer): `ack=<ms|->` the instant
//! acknowledgement became audible (pipeline/ack.rs; before this field it was counted as `audio`: a line with `audio`
//! before `speak` is an ack), and `voice_end=<ms|->` Silero's last voiced audio (the `SpeechEnd` time), usually 0 or
//! negative: the speech end above is the later of it and the recognizer's last word end, so the detector's real pause
//! is `endpoint - voice_end`. Then `first_text=<ms|->` the first non-empty visible text (`text` is the first text
//! event, which may be empty) and `tts=<ms|->` speech synthesis started for the first queued line (after the reply
//! hold, bridge/hold.rs); `speak` is the first speakable line handed to the speech queue.
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Endpoint,
    SmartTurn,
    Asr,
    Final,
    Admit,
    Decide,
    Deliver,
    Request,
    Stream,
    Think,
    Text,
    ToolCall,
    Tool,
    Speak,
    Audio,
    Done,
}
impl Stage {
    pub const ALL: [Stage; 16] = [
        Stage::Endpoint, Stage::SmartTurn, Stage::Asr, Stage::Final, Stage::Admit, Stage::Decide, Stage::Deliver,
        Stage::Request, Stage::Stream, Stage::Think, Stage::Text, Stage::ToolCall, Stage::Tool, Stage::Speak,
        Stage::Audio, Stage::Done,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Stage::Endpoint => "endpoint",
            Stage::SmartTurn => "smartturn",
            Stage::Asr => "asr",
            Stage::Final => "final",
            Stage::Admit => "admit",
            Stage::Decide => "decide",
            Stage::Deliver => "deliver",
            Stage::Request => "request",
            Stage::Stream => "stream",
            Stage::Think => "think",
            Stage::Text => "text",
            Stage::ToolCall => "toolcall",
            Stage::Tool => "tool",
            Stage::Speak => "speak",
            Stage::Audio => "audio",
            Stage::Done => "done",
        }
    }
}

/// One voice request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub id: String,
    pub source: String,
    pub speech_end_ns: u64,
    pub forced: bool,
    pub trigger: Option<String>,
    marks: [Option<u64>; 16],
    pub calls: u32,
    pub busy: bool,
    pub first_tool: Option<String>,
    /// The instant acknowledgement became audible (not the answer).
    pub ack_ns: Option<u64>,
    /// Silero's last voiced audio (`listen::timing::FinalTiming::voice_end_ns`).
    pub voice_end_ns: Option<u64>,
    /// The first non-empty text delta.
    pub first_text_ns: Option<u64>,
    /// Speech synthesis started (the first line's).
    pub tts_ns: Option<u64>,
}
impl Record {
    pub fn mark(&self, stage: Stage) -> Option<u64> {
        self.marks[stage as usize]
    }
    /// The first time only: later events of the same stage never move a mark.
    pub fn set(&mut self, stage: Stage, ns: u64) {
        self.marks[stage as usize].get_or_insert(ns);
    }
    fn ms(&self, ns: u64) -> i64 {
        (ns as i128 - self.speech_end_ns as i128).div_euclid(1_000_000) as i64
    }
    /// The one log line (see the module docs).
    pub fn line(&self, settled: &str) -> String {
        let clean = |s: &str| s.chars().map(|c| if c.is_whitespace() || c == '=' { '_' } else { c }).collect::<String>();
        let mut out = format!(
            "latency v=1 id={} trigger={} forced={} calls={} busy={} first_tool={} settled={}",
            clean(&self.id),
            self.trigger.as_deref().map_or("-".into(), clean),
            self.forced as u8,
            self.calls,
            self.busy as u8,
            self.first_tool.as_deref().map_or("-".into(), clean),
            settled
        );
        for stage in Stage::ALL {
            match self.mark(stage) {
                Some(ns) => out.push_str(&format!(" {}={}", stage.key(), self.ms(ns))),
                None => out.push_str(&format!(" {}=-", stage.key())),
            }
        }
        match self.mark(Stage::Audio).or(self.mark(Stage::Done)) {
            Some(ns) => out.push_str(&format!(" total={}", self.ms(ns))),
            None => out.push_str(" total=-"),
        }
        for (key, at) in [("ack", self.ack_ns), ("voice_end", self.voice_end_ns), ("first_text", self.first_text_ns), ("tts", self.tts_ns)] {
            match at {
                Some(ns) => out.push_str(&format!(" {key}={}", self.ms(ns))),
                None => out.push_str(&format!(" {key}=-")),
            }
        }
        out
    }
}

/// How long a finished turn waits for its first audio before it is logged without it.
const AUDIO_WAIT_NS: u64 = 30_000_000_000;
const PENDING: usize = 32;

/// Finals waiting for a decision, and the request Heimdall is answering. Pure: callers pass the clock; each call
/// returns the lines that settled.
#[derive(Debug, Default)]
pub struct Tracker {
    received: Vec<Record>,
    pending: Vec<Record>,
    current: Option<Record>,
    /// The last `RECENT_REPLIES` settled requests' first speech (`speak`, ms after his last word), oldest first.
    replies: std::collections::VecDeque<u64>,
}
/// How many recent replies the estimate of the next one looks at (`expected_reply_ms`).
const RECENT_REPLIES: usize = 5;
impl Tracker {
    /// The current request's line; its first speech joins the recent replies.
    fn settle(&mut self) -> String {
        let r = self.current.take().unwrap();
        if let Some(ms) = r.mark(Stage::Speak).and_then(|ns| ns.checked_sub(r.speech_end_ns)).map(|ns| ns / 1_000_000) {
            if self.replies.len() == RECENT_REPLIES {
                self.replies.pop_front();
            }
            self.replies.push_back(ms);
        }
        r.line("done")
    }
    /// When the next reply's first speech is likely (ms after his last word): the median of the recent ones, with at
    /// least three of them; None otherwise. The ack skips itself when a reply is that close (pipeline/ack.rs).
    pub fn expected_reply_ms(&self) -> Option<u64> {
        if self.replies.len() < 3 {
            return None;
        }
        let mut sorted: Vec<u64> = self.replies.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }
    /// The conductor received a final from listen (`listen` marks: endpoint, Smart Turn and decode times).
    pub fn received(&mut self, source: &str, end_ns: u64, forced: bool, listen: Option<listen::timing::FinalTiming>, now: u64) {
        // A final held behind a spoken command re-enters the conductor later: keep the first receipt and its marks.
        if self.received.iter().any(|x| x.source == source && x.speech_end_ns == end_ns) {
            return;
        }
        let mut r = Record { source: source.into(), speech_end_ns: end_ns, forced, ..Default::default() };
        if let Some(t) = listen {
            r.set(Stage::Endpoint, t.endpoint_ns);
            r.set(Stage::SmartTurn, t.turn_ns);
            r.set(Stage::Asr, t.decoded_ns);
            r.voice_end_ns = (t.voice_end_ns != 0).then_some(t.voice_end_ns);
        }
        r.set(Stage::Final, now);
        self.received.retain(|x| x.source != source);
        self.received.push(r);
        if self.received.len() > PENDING {
            self.received.remove(0);
        }
    }
    /// The conductor admitted the final as utterance `id` (the id the decision and the wake carry).
    pub fn admitted(&mut self, id: &str, source: &str, end_ns: u64, forced: bool, now: u64) {
        let r = match self.received.iter().position(|r| r.source == source && r.speech_end_ns == end_ns) {
            Some(i) => self.received.remove(i),
            // A final that never passed `received` (a call's split final): no listen marks.
            None => Record { source: source.into(), speech_end_ns: end_ns, forced, ..Default::default() },
        };
        let mut r = Record { id: id.into(), ..r };
        r.set(Stage::Admit, now);
        self.pending.retain(|x| x.id != id);
        self.pending.push(r);
        if self.pending.len() > PENDING {
            self.pending.remove(0);
        }
    }
    pub fn decided(&mut self, id: &str, trigger: Option<&str>, now: u64) {
        if let Some(r) = self.pending.iter_mut().chain(self.current.iter_mut()).find(|r| r.id == id) {
            r.set(Stage::Decide, now);
            if r.trigger.is_none() {
                r.trigger = trigger.map(str::to_owned);
            }
        }
    }
    /// The wake for utterance `id` reached Pi. `busy`: a turn was already running. A request still being answered is
    /// logged now (`superseded`); the stream that follows belongs to the new one.
    pub fn delivered(&mut self, id: &str, busy: bool, now: u64) -> Vec<String> {
        let mut lines = vec![];
        let Some(i) = self.pending.iter().position(|r| r.id == id) else { return lines };
        let mut r = self.pending.remove(i);
        r.set(Stage::Deliver, now);
        r.busy = busy;
        if let Some(old) = self.current.take() {
            lines.push(old.line(if old.mark(Stage::Done).is_some() { "done" } else { "superseded" }));
        }
        self.current = Some(r);
        lines
    }
    /// A Pi RPC event (`Event::Other`).
    pub fn agent_event(&mut self, v: &serde_json::Value, now: u64) {
        let Some(r) = self.current.as_mut().filter(|r| r.mark(Stage::Done).is_none()) else { return };
        match v["type"].as_str() {
            Some("turn_start") => {
                r.calls += 1;
                r.set(Stage::Request, now);
            }
            Some("message_start") if v["message"]["role"] == "assistant" => r.set(Stage::Stream, now),
            Some("message_update") => match v["assistantMessageEvent"]["type"].as_str() {
                Some("thinking_start" | "thinking_delta") => r.set(Stage::Think, now),
                Some(kind @ ("text_start" | "text_delta")) => {
                    r.set(Stage::Text, now);
                    if kind == "text_delta" && v["assistantMessageEvent"]["delta"].as_str().is_some_and(|d| !d.trim().is_empty()) {
                        r.first_text_ns.get_or_insert(now);
                    }
                }
                Some("toolcall_start" | "toolcall_delta") => r.set(Stage::ToolCall, now),
                _ => {}
            },
            Some("tool_execution_start") => {
                if r.mark(Stage::Tool).is_none() {
                    r.first_tool = v["toolName"].as_str().map(|n| n.trim_start_matches("mcp__heimdall__").to_owned());
                }
                r.set(Stage::Tool, now);
            }
            _ => {}
        }
    }
    /// The turn answering the current request completed. Logged now if Heimdall's voice was already heard; otherwise
    /// at its first audio (a reply spoken after the turn) or, for a silent turn, `AUDIO_WAIT_NS` later.
    pub fn completed(&mut self, now: u64) -> Vec<String> {
        let Some(r) = self.current.as_mut() else { return vec![] };
        r.set(Stage::Done, now);
        if r.mark(Stage::Audio).is_none() {
            return vec![];
        }
        vec![self.settle()]
    }
    /// Speech synthesis started for a queued line.
    pub fn tts_started(&mut self, now: u64) {
        if let Some(r) = self.current.as_mut().filter(|r| r.mark(Stage::Speak).is_some()) {
            r.tts_ns.get_or_insert(now);
        }
    }
    pub fn speak_queued(&mut self, now: u64) {
        if let Some(r) = self.current.as_mut() {
            r.set(Stage::Speak, now);
        }
    }
    /// The instant acknowledgement became audible: kept apart from the answer's `audio`.
    pub fn ack_audible(&mut self, now: u64) {
        if let Some(r) = self.current.as_mut().filter(|r| r.mark(Stage::Audio).is_none()) {
            r.ack_ns.get_or_insert(now);
        }
    }
    /// Heimdall's answer became audible.
    pub fn audio(&mut self, now: u64) -> Vec<String> {
        let mut lines = vec![];
        let Some(r) = self.current.as_mut() else { return lines };
        if r.mark(Stage::Audio).is_some() {
            return lines;
        }
        r.set(Stage::Audio, now);
        if r.mark(Stage::Done).is_some() {
            lines.push(self.settle());
        }
        lines
    }
    /// A finished turn that stayed silent for `AUDIO_WAIT_NS` is logged without audio. Every entry point runs it.
    pub fn expire(&mut self, now: u64) -> Vec<String> {
        match &self.current {
            Some(r) if r.mark(Stage::Done).is_some_and(|d| now.saturating_sub(d) > AUDIO_WAIT_NS) => {
                vec![self.settle()]
            }
            _ => vec![],
        }
    }
    #[cfg(test)]
    pub fn current(&self) -> Option<&Record> {
        self.current.as_ref()
    }
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);
fn with(f: impl FnOnce(&mut Tracker, u64) -> Vec<String>) {
    let lines = {
        let mut guard = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        let t = guard.get_or_insert_with(Tracker::default);
        let now = capture::host_now_ns();
        let mut lines = t.expire(now);
        lines.extend(f(t, now));
        lines
    };
    for line in lines {
        eprintln!("{line}");
    }
}
pub fn received(source: &str, end_ns: u64, forced: bool) {
    let listen = listen::timing::take(source, end_ns);
    with(|t, now| {
        t.received(source, end_ns, forced, listen, now);
        vec![]
    });
}
pub fn admitted(id: &str, source: &str, end_ns: u64, forced: bool) {
    with(|t, now| {
        t.admitted(id, source, end_ns, forced, now);
        vec![]
    });
}
pub fn decided(id: &str, trigger: Option<&str>) {
    with(|t, now| {
        t.decided(id, trigger, now);
        vec![]
    });
}
pub fn delivered(id: &str, busy: bool) {
    with(|t, now| t.delivered(id, busy, now));
}
pub fn agent_event(event: &agent::protocol::Event) {
    match event {
        agent::protocol::Event::Other(v) => with(|t, now| {
            t.agent_event(v, now);
            vec![]
        }),
        agent::protocol::Event::Complete { query: true, .. } => with(|t, now| t.completed(now)),
        _ => {}
    }
}
pub fn speak_queued() {
    with(|t, now| {
        t.speak_queued(now);
        vec![]
    });
}
pub fn audio() {
    with(|t, now| t.audio(now));
}
pub fn tts_started() {
    with(|t, now| {
        t.tts_started(now);
        vec![]
    });
}
pub fn ack_audible() {
    with(|t, now| {
        t.ack_audible(now);
        vec![]
    });
}
/// `Tracker::expected_reply_ms` for the live requests.
pub fn expected_reply_ms() -> Option<u64> {
    TRACKER.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(Tracker::expected_reply_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const S: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;
    fn listen_marks(end: u64) -> listen::timing::FinalTiming {
        listen::timing::FinalTiming { end_ns: end, endpoint_ns: end + 1500 * MS, turn_ns: end + 1530 * MS, decoded_ns: end + 1700 * MS, voice_end_ns: 0 }
    }
    /// Complete the turn and let the audio wait pass: the line of a turn that stays silent.
    fn settle(t: &mut Tracker, at: u64) -> String {
        assert!(t.completed(at).is_empty());
        t.expire(at + AUDIO_WAIT_NS + 1).pop().unwrap()
    }
    fn field<'a>(line: &'a str, key: &str) -> &'a str {
        line.split(' ').find_map(|kv| kv.strip_prefix(&format!("{key}="))).unwrap()
    }
    /// A typical voice request: d1 send, one model call that speaks, a second that writes the final text.
    #[test]
    fn a_spoken_request_logs_every_stage_in_order_once_audio_is_heard() {
        let end = 100 * S;
        let mut t = Tracker::default();
        t.received("mic:a", end, false, Some(listen_marks(end)), end + 1710 * MS);
        t.admitted("u:1", "mic:a", end, false, end + 1712 * MS);
        t.decided("u:1", Some("d1"), end + 1790 * MS);
        assert!(t.delivered("u:1", false, end + 1800 * MS).is_empty());
        t.agent_event(&json!({"type":"turn_start"}), end + 1830 * MS);
        t.agent_event(&json!({"type":"message_start","message":{"role":"assistant"}}), end + 3000 * MS);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"h"}}), end + 3100 * MS);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_start"}}), end + 8000 * MS);
        t.agent_event(&json!({"type":"tool_execution_start","toolName":"mcp__heimdall__speak"}), end + 9000 * MS);
        t.speak_queued(end + 9010 * MS);
        t.agent_event(&json!({"type":"turn_start"}), end + 9050 * MS);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"x"}}), end + 12000 * MS);
        assert!(t.completed(end + 16000 * MS).is_empty(), "queued speech not yet audible: wait for it");
        let lines = t.audio(end + 16500 * MS);
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        // lab/latency/test_map.py parses this exact line (its `SPOKEN`): keep the two in step.
        assert_eq!(l, "latency v=1 id=u:1 trigger=d1 forced=0 calls=2 busy=0 first_tool=speak settled=done endpoint=1500 \
smartturn=1530 asr=1700 final=1710 admit=1712 decide=1790 deliver=1800 request=1830 stream=3000 think=3100 text=12000 \
toolcall=8000 tool=9000 speak=9010 audio=16500 done=16000 total=16500 ack=- voice_end=- first_text=12000 tts=-");
        for (k, v) in [("endpoint", "1500"), ("smartturn", "1530"), ("asr", "1700"), ("final", "1710"), ("admit", "1712"),
            ("decide", "1790"), ("deliver", "1800"), ("request", "1830"), ("stream", "3000"), ("think", "3100"),
            ("text", "12000"), ("toolcall", "8000"), ("tool", "9000"), ("speak", "9010"), ("audio", "16500"),
            ("done", "16000"), ("total", "16500")] {
            assert_eq!(field(l, k), v, "{k} in {l}");
        }
        assert!(t.current().is_none());
    }
    /// The ack's estimate of the next reply (pipeline/ack.rs): the median first speech of the last five, from three on.
    #[test]
    fn the_expected_reply_is_the_median_of_recent_first_speech() {
        let mut t = Tracker::default();
        for (i, speak_ms) in [12_000u64, 2_000, 3_000, 40_000, 2_500, 1_500].into_iter().enumerate() {
            let end = (i as u64 + 1) * 100 * S;
            let id = format!("u:{i}");
            t.received("mic:a", end, false, None, end + S);
            t.admitted(&id, "mic:a", end, false, end + S);
            t.delivered(&id, false, end + S);
            t.speak_queued(end + speak_ms * MS);
            t.audio(end + speak_ms * MS + 100 * MS);
            assert_eq!(t.completed(end + 50 * S).len(), 1);
            if i == 1 {
                assert_eq!(t.expected_reply_ms(), None, "two are not enough");
            }
        }
        assert_eq!(t.expected_reply_ms(), Some(2_500), "the last five: 2.0, 3.0, 40, 2.5, 1.5 s");
    }
    #[test]
    fn a_final_held_behind_a_spoken_command_keeps_its_first_receipt() {
        let end = 10 * S;
        let mut t = Tracker::default();
        t.received("mic:a", end, false, Some(listen_marks(end)), end + 1710 * MS);
        // Re-entered after the command check: listen marks were already taken, so this call has none.
        t.received("mic:a", end, false, None, end + 2500 * MS);
        t.admitted("u:8", "mic:a", end, false, end + 2510 * MS);
        t.delivered("u:8", false, end + 2520 * MS);
        let l = settle(&mut t, end + 9 * S);
        assert!(l.contains(" asr=1700 ") && l.contains(" final=1710 ") && l.contains(" admit=2510 "), "{l}");
    }
    #[test]
    fn a_silent_turn_is_logged_after_the_audio_wait_with_missing_stages_as_dashes() {
        let mut t = Tracker::default();
        t.received("mic:a", 10 * S, true, None, 13 * S);
        t.admitted("u:2", "mic:a", 10 * S, true, 13 * S);
        t.delivered("u:2", false, 13 * S);
        t.agent_event(&json!({"type":"turn_start"}), 13 * S);
        assert!(t.completed(20 * S).is_empty(), "a reply may still be spoken after the turn");
        assert!(t.expire(20 * S + AUDIO_WAIT_NS).is_empty());
        let l = settle(&mut t, 20 * S);
        assert_eq!((field(&l, "forced"), field(&l, "trigger"), field(&l, "endpoint"), field(&l, "speak"), field(&l, "audio")), ("1", "-", "-", "-", "-"));
        assert_eq!((field(&l, "done"), field(&l, "total")), ("10000", "10000"));
    }
    #[test]
    fn a_request_delivered_mid_turn_supersedes_the_open_one_and_takes_the_stream() {
        let mut t = Tracker::default();
        for (id, end) in [("u:3", 10 * S), ("u:4", 20 * S)] {
            t.received("mic:a", end, false, None, end + S);
            t.admitted(id, "mic:a", end, false, end + S);
        }
        t.delivered("u:3", false, 11 * S);
        t.agent_event(&json!({"type":"turn_start"}), 11 * S);
        let lines = t.delivered("u:4", true, 21 * S);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("id=u:3") && lines[0].contains("settled=superseded"), "{}", lines[0]);
        t.agent_event(&json!({"type":"turn_start"}), 22 * S);
        let l = settle(&mut t, 25 * S);
        assert!(l.contains("id=u:4") && l.contains("busy=1") && l.contains("calls=1") && l.contains("request=2000"), "{l}");
    }
    #[test]
    fn events_without_a_voice_request_and_repeated_stages_change_nothing() {
        let mut t = Tracker::default();
        t.agent_event(&json!({"type":"turn_start"}), S);
        assert!(t.completed(2 * S).is_empty() && t.audio(3 * S).is_empty());
        t.speak_queued(S);
        // A wake for an utterance never admitted (a typed message, a notice) starts no record.
        assert!(t.delivered("typed", false, S).is_empty() && t.current().is_none());
        t.received("mic:a", 10 * S, false, None, 11 * S);
        t.admitted("u:5", "mic:a", 10 * S, false, 11 * S);
        t.delivered("u:5", false, 12 * S);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}), 13 * S);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta"}}), 14 * S);
        // A user echo is not the provider's stream.
        t.agent_event(&json!({"type":"message_start","message":{"role":"user"}}), 14 * S);
        assert_eq!(t.current().unwrap().mark(Stage::Text), Some(13 * S));
        assert_eq!(t.current().unwrap().mark(Stage::Stream), None);
    }
    #[test]
    fn speech_after_the_turn_settles_it_and_speech_that_never_plays_is_logged_after_the_wait() {
        let mut t = Tracker::default();
        t.received("mic:a", 10 * S, false, None, 11 * S);
        t.admitted("u:6", "mic:a", 10 * S, false, 11 * S);
        t.delivered("u:6", false, 12 * S);
        t.speak_queued(13 * S);
        assert!(t.completed(14 * S).is_empty());
        assert!(t.audio(14 * S + AUDIO_WAIT_NS / 2).len() == 1, "audio inside the wait settles it");
        t.received("mic:a", 50 * S, false, None, 51 * S);
        t.admitted("u:7", "mic:a", 50 * S, false, 51 * S);
        t.delivered("u:7", false, 52 * S);
        t.speak_queued(53 * S);
        t.completed(54 * S);
        let lines = t.expire(54 * S + AUDIO_WAIT_NS + S);
        assert!(lines.len() == 1 && lines[0].contains("settled=done") && lines[0].contains("audio=-") && lines[0].contains("speak=3000"), "{lines:?}");
        assert!(t.current().is_none());
    }
    #[test]
    fn a_final_admitted_without_listen_marks_and_ids_with_spaces_stay_one_token() {
        let mut t = Tracker::default();
        t.admitted("odd id=x", "call", 10 * S, false, 11 * S);
        t.decided("odd id=x", Some("name"), 11 * S);
        t.delivered("odd id=x", false, 12 * S);
        let l = settle(&mut t, 13 * S);
        assert!(l.contains(" id=odd_id_x ") && l.contains(" admit=1000 ") && l.contains(" final=- "), "{l}");
        assert_eq!(l.split(' ').filter(|kv| kv.contains('=')).count(), 8 + 16 + 1 + 4, "v, id, trigger, forced, calls, busy, first_tool, settled; 16 stages; total; ack, voice_end, first_text, tts");
    }
    /// The metric is the end of his speech to the first audible actual answer. The acknowledgement
    /// is logged as `ack`, never as the answer's `audio`; the detector's last voiced audio rides along as `voice_end`.
    #[test]
    fn the_ack_is_not_the_answer_and_the_last_voice_is_logged() {
        let end = 100 * S;
        let mut t = Tracker::default();
        let marks = listen::timing::FinalTiming { voice_end_ns: end - 400 * MS, ..listen_marks(end) };
        t.received("mic:a", end, false, Some(marks), end + 1710 * MS);
        t.admitted("u:1", "mic:a", end, false, end + 1712 * MS);
        t.delivered("u:1", false, end + 1800 * MS);
        t.ack_audible(end + 1850 * MS);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":" "}}), end + 5000 * MS);
        t.agent_event(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"Done"}}), end + 5100 * MS);
        t.tts_started(end + 8000 * MS);
        t.speak_queued(end + 9000 * MS);
        t.tts_started(end + 9050 * MS);
        assert!(t.completed(end + 9500 * MS).is_empty());
        let l = t.audio(end + 9700 * MS).pop().unwrap();
        assert_eq!((field(&l, "audio"), field(&l, "ack"), field(&l, "voice_end"), field(&l, "total")), ("9700", "1850", "-400", "9700"), "{l}");
        assert_eq!((field(&l, "text"), field(&l, "first_text"), field(&l, "tts")), ("5000", "5100", "9050"), "an empty delta is not visible text; tts counts after the line is queued");
    }
    #[test]
    fn a_mark_before_the_speech_end_is_negative_not_wrapped() {
        let mut r = Record { speech_end_ns: 10 * S, ..Default::default() };
        r.set(Stage::Final, 10 * S - 2 * MS);
        assert!(r.line("done").contains(" final=-2 "));
    }
}
