//! When Heimdall makes a sound, and when it keeps quiet (night pass 3, 21 Oct).
//!
//! **Wake sound.** the user (21 Oct, 00:17): "whenever the decider decides to send a message to the agent, there should
//! be a little ring noise, some sound to indicate that the message was actually sent." A spoken wake (his name, "send
//! it", the follow-up window or d1's send; `Pipeline::dispatch_pending`) plays macOS's own Tink, quietly, and so does
//! a typed message (bridge/typed_arrival.rs, 30 Oct); the Agent-off dry run does not. Settings `wake_sound`, the menu's Speaking ▸ Wake Sound, on by default.
//! The cue is Heimdall's own sound on the microphone, kept out the way its voice is: from the moment Tink starts on
//! the main queue (the play request until then) to `CUE_WINDOW` later it counts as Heimdall audible, so speech
//! starting then is not the user's turn (`hold`), and a microphone utterance lying wholly inside that window is archived
//! only, never a transcript turn, a decision or a wake (pipeline/utterances.rs).
//!
//! **Who may talk in a call** (bug note 2026-11-05T02-32-17: speak only in a quiet gap, and stop as soon as anyone
//! talks, guests included). A voice opening on a call source (`other_voice`) is reported to the app
//! (`set_call_voice_hook`): a reply waiting to start waits for the gap (`hold::OwnerVoice::other_voice`), and one
//! audible when the guest's voice has lasted `GUEST_CUT_AFTER` stops (`Tools::guest_talked_over`). Heimdall's own
//! voice never reaches a call source's tap (it plays on the Mac, the tap carries the call app's output), and the
//! `GUEST_CUT_AFTER` wait passes a click, a cough or a blip of echo from a guest's open speakers.
//! While the user is in a call Heimdall also speaks quieter (`CALL_VOICE_GAIN`, bug note 2026-11-05T00-10-37: it was
//! too loud).
//!
//! **In a conversation.** the user (20 Oct, 23:09): "when I'm talking to someone I don't want you to talk when a worker
//! updated or whatever, you just ask me a question." the user is in a conversation while the call shows it is live: a call app's
//! line with words (Discord, Zoom, Slack, Teams, FaceTime: `context_policy::CALL_BUNDLES`, diarized or still waiting
//! for its speaker) ended at most `CONVERSATION_NS` (45 s) ago, or another participant is talking on a call source now
//! (its voice is open, `other_voice`). It ends at once with a goodbye in the call or the user's "I'm off the call" / "you
//! can talk to me now" (`decide::send::{farewell, off_call}`), and `CALL_SOURCE_GONE` after the last call source
//! stops playing (a stalled tap, App Audio switched off or sleep leave only the window). Bug 2026-11-04T17-48-24: the window was the decider's two minutes (`decide::send::CALL_ACTIVE_NS`), so
//! after Angel's "Sorry, bro, take care. Good luck." at 17:46:29 Heimdall held every reply until 17:48:29 (20 Oct, every
//! in-call trigger came 0-36 s after a call line, the nearest out-of-call one 157 s after).
//! Meanwhile no `speak` call and no final reply is played: the text is shown silently on the island, it joins the
//! decider's conversation like any island line, and a `speak` call's result tells the agent in one line
//! (`HELD_IN_CONVERSATION`). Speech resumes by itself once no call line has ended for that long. The exception is
//! The user's own standing rule (prompt, 20 Oct: "Speak only when he clearly addresses you by name with a question that
//! needs a spoken answer, and then briefly"): a run woken by his microphone line addressing Heimdall by name
//! (`decide::send::addressed_by_name`) may speak until it settles. Runs woken by notices, T3 or worker results,
//! scheduled wakes, or his words that did not address Heimdall by name are held.
//!
//! A line held in a call is not lost (47 held lines in 3 days, 46 on 4 Nov, each shown on the island and never heard):
//! the latest `HELD_LINES` wait, and once the conversation is over (the call window, or a goodbye), the user is quiet,
//! Speaking is on, Heimdall is silent and the agent on, they are said through the normal path, each once, with no
//! lead-in (`mcp_actions::Host::replay_held`, checked on every capture event from `Tools::speech_status`). A line is
//! dropped instead when it is older than `HELD_FOR`, or a newer reply was queued for speech after it. Bug note
//! 2026-11-05T02-32-37: a voice line minutes late has no point. A held line is shown on the island at once and is said only in the next quiet gap within `HELD_FOR` (a minute); past it,
//! it stays text only, after the call too (held lines were said 136-472 s late on 4-5 Nov, with `HELD_FOR` 600 s). When the user's own
//! words reach the agent after a line was held (his wake: a line the decider sent, his name, a typed message, a drop
//! with his words), he is talking to Heimdall: the newest such line is said as soon as he is quiet, in a call or not
//! (while nobody else is talking on the call right then), unless a newer reply is queued first, and the older ones are
//! dropped. A typed message stops what is playing but keeps the held lines for this (`Tools::typed_arrived`) (bug 2026-11-04T17-48-24: three replies were
//! dropped unheard as he kept asking "Heimdall, hello?"). Every held line was shown on the island when it was held, so
//! a dropped one is still in front of him. One log line per replay and per drop.
//!
//! `ask` cards are never read aloud ("the text to speech by default reads those typed questions, which
//! is kind of annoying"); they show silently in every mode (mcp_actions.rs).
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

/// The cue: macOS's Tink, a soft 0.56-second tick, at a quarter of the output volume.
#[cfg_attr(test, allow(dead_code))]
pub(crate) const CUE_SOUND: &str = "Tink";
#[cfg_attr(test, allow(dead_code))]
pub(crate) const CUE_VOLUME: f32 = 0.25;
/// The cue counts as Heimdall's own sound from its start for this long: Tink's 0.56 s, the output and
/// microphone latency (tens of ms), and the room's tail.
pub(crate) const CUE_WINDOW: Duration = Duration::from_millis(1_000);
/// A conversation lasts while a call line ended this recently: past every in-call trigger of 20 Oct (0-36 s after a
/// call line), well short of the decider's two minutes, which kept Heimdall quiet after the call (bug
/// 2026-11-04T17-48-24).
pub(crate) const CONVERSATION_NS: u64 = 45_000_000_000;
/// The last call source stopped this long ago: the call is over. A route change brings a source back within about 100 ms
/// (17:41:16-17:41:17, 33 and 70 ms), so it never ends a call.
pub(crate) const CALL_SOURCE_GONE: Duration = Duration::from_secs(3);
/// A call source's voice open longer than this without its end proves nothing more (a detector that missed the end).
pub(crate) const OPEN_VOICE_MAX: Duration = Duration::from_secs(60);
/// What a `speak` call returns while the user is in a conversation: one line, and what to do instead.
pub(crate) const HELD_IN_CONVERSATION: &str = "Not spoken: User is talking with other people, so it was shown silently on the island; it is said only if he talks to you or the call ends within a minute, unless you reply again first, and otherwise stays as text. Until the conversation ends, nothing you say is played unless he addresses you by name; when he needs to answer, use `ask` with two to four short options.";
/// Heimdall's voice level while the user is in a call (bug note 2026-11-05T00-10-37: it was too loud): half the
/// amplitude, about 6 dB down, under the call's voices instead of over them.
pub(crate) const CALL_VOICE_GAIN: f32 = 0.5;
/// A guest's voice on a call source must last this long while Heimdall is audible before it stops him: past a click,
/// a cough or a blip of echo, well inside "stop as soon as anyone talks" (bug note 2026-11-05T02-32-17).
pub(crate) const GUEST_CUT_AFTER: Duration = Duration::from_millis(400);
/// What the app does when a voice opens (`Some(start)`) or ends (`None`) on call source `source` (`set_call_voice_hook`).
pub(crate) type CallVoiceHook = Arc<dyn Fn(&str, Option<u64>) + Send + Sync>;

/// The user's `words` ask Heimdall to listen to `target`: a "listen" or "słuchaj" not negated just before it ("stop
/// listening", "don't listen", "nie słuchaj"), followed within a few words by the target's name ("listen to Vim and
/// angel sister"), or at once by him, her or them ("listen to him", "posłuchaj go"). A "not" or "but" ends the list
/// ("listen to Vim, not Dave").
pub(crate) fn asks_to_listen(words: &str, target: &str) -> bool {
    const NOT: &[&str] = &["stop", "don", "dont", "t", "not", "never", "quit", "no", "nie"];
    const PRONOUNS: &[&str] = &["him", "her", "them", "jego", "go", "ją", "nią", "nia", "ich", "nich"];
    const ENDS: &[&str] = &["not", "nie", "but", "ale", "except", "oprócz"];
    let tokens: Vec<String> = words.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect();
    let name: Vec<String> = target.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect();
    let listen = |t: &str| t.starts_with("listen") || ["słuchaj", "sluchaj", "posłuchaj", "posluchaj", "słuchać", "sluchac"].iter().any(|k| t.starts_with(k));
    tokens.iter().enumerate().any(|(i, t)| {
        if !listen(t) || tokens[i.saturating_sub(2)..i].iter().any(|p| NOT.contains(&p.as_str())) {
            return false;
        }
        let after: Vec<&str> = tokens[i + 1..].iter().map(String::as_str).take_while(|w| !ENDS.contains(w)).take(8).collect();
        let first = after.iter().copied().find(|w| !matches!(*w, "to" | "also" | "too" | "do"));
        let named = !name.is_empty() && after.windows(name.len()).any(|w| w.iter().zip(&name).all(|(a, b)| *a == b.as_str()));
        named || first.is_some_and(|w| PRONOUNS.contains(&w))
    })
}

/// Plays the cue. Production: Tink through AppKit on the main queue; tests count instead and never play.
pub(crate) type CuePlayer = Arc<dyn Fn() + Send + Sync>;

pub(crate) struct SpeechPolicy {
    /// Settings `wake_sound`.
    cue_on: AtomicBool,
    /// Settings `speak_replies` (`cue_allowed`).
    speaking_on: AtomicBool,
    /// Tests: the default output is Bluetooth.
    #[cfg(test)]
    test_bluetooth: AtomicBool,
    /// Host clock of the last cue's start (0: none): its play request, then the moment it actually started playing
    /// (the main queue may run it later; `cue_started`).
    cue_at_ns: Arc<AtomicU64>,
    player: Mutex<CuePlayer>,
    /// Host clock of the latest end of a call line with words (0: none).
    other_line_ns: AtomicU64,
    /// Call sources whose voice is open now (another participant talking), with the host clock it started.
    other_voice: Mutex<std::collections::HashMap<String, u64>>,
    /// Host clock the last call source stopped (0: one is open, or none stopped).
    call_gone_ns: AtomicU64,
    /// A goodbye in the call (or the user's "I'm off the call") ended then: the conversation is over
    /// (`decide::send::call_active_within`).
    farewell_ns: AtomicU64,
    /// The agent's current run was woken by the user addressing Heimdall by name: its replies may be spoken during a
    /// conversation. Cleared when the run settles, the user stops it, or the agent starts. Shared with the skipped
    /// reply's retry, which sets it as it wakes the agent (`addressed_flag`).
    addressed: Arc<AtomicBool>,
    /// The app's reaction to a voice on a call source (`set_call_voice_hook`; none in unit tests).
    call_voice_hook: Mutex<Option<CallVoiceHook>>,
    /// Host-clock spans of Heimdall's own short sounds besides the cue (instant acknowledgements, pipeline/ack.rs),
    /// newest last, at most `OWN_SOUNDS`.
    /// Each is (opens, sounds until, closes): see `own_sound_until`.
    own: Mutex<std::collections::VecDeque<(u64, u64, u64)>>,
    /// Sentences Heimdall played through speakers, newest last, at most `PLAYED`: the echo guard's reference
    /// (pipeline/echo_guard.rs). The last one stays open (`u64::MAX`) until the next starts or the voice stops.
    played: Mutex<std::collections::VecDeque<Played>>,
    /// Host clock until which Apple's echo cancellation is wanted (0: never): every played sentence keeps it on for
    /// `ECHO_CANCEL_TAIL` after the voice stops (bridge/echo_cancel.rs). Never ahead of a wake: the unit ducks and
    /// looks like a call, so it waits for a reply that will really be spoken (review 23 Oct).
    echo_cancel_until: AtomicU64,
    /// Settings `echo_cancel_off`.
    echo_cancel_off: AtomicBool,
    /// Who woke the agent's current run, for `listen_to` (pipeline/listen_to.rs): the user's own words with their host
    /// time, and whether a guest's line woke or steered it. Cleared when the run settles.
    run_voices: Mutex<RunVoices>,
    /// Lines held in a conversation, oldest first, at most `HELD_LINES`: said once it is over (`due_held`).
    held: Mutex<std::collections::VecDeque<HeldLine>>,
    /// Host clock of the latest reply queued for speech (0: none). Lines held before it are superseded.
    reply_queued_ns: AtomicU64,
    /// Host clock of the user's latest words that reached the agent (0: none). Lines held before them are superseded.
    owner_words_ns: AtomicU64,
}
/// A reply held because the user was in a conversation, kept to be said once it is over.
#[derive(Clone)]
pub(crate) struct HeldLine {
    pub(crate) text: String,
    pub(crate) language: String,
    /// Host clock when it was held.
    pub(crate) held_ns: u64,
    /// The delivery epoch it was said in: Agent Off or Heimdall Off since then means it is never said (review 4 Nov).
    pub(crate) permit: agent::delivery::DeliveryPermit,
}
/// A held line `due_held` gives back to say now. `addressed`: the user's words reached the agent after it was held, so it is
/// said even in a conversation.
pub(crate) struct Due {
    pub(crate) line: HeldLine,
    pub(crate) addressed: bool,
}
/// Held lines kept for after the conversation; the oldest goes first past it.
pub(crate) const HELD_LINES: usize = 4;
/// A held line older than this is never said, in the call or after it: it stays as the text shown when it was held.
/// Bug note 2026-11-05T02-32-37: speak it within about 30-60 s, otherwise leave it as text only.
/// Was 600 s: held lines were said 136-472 s late (20:51:34 178 s, 00:21:01 372 s, bug note 2026-11-05T00-14-40).
pub(crate) const HELD_FOR: Duration = Duration::from_secs(60);
#[derive(Default)]
struct RunVoices {
    owner: Vec<(u64, String)>,
    guest: bool,
    /// The utterance ID of the microphone line whose wake (a decider send, a wake key, his name) last woke or steered
    /// this run: the line the island's Stop labels (pipeline/hotkeys.rs).
    line: Option<String>,
    /// The user pressed Stop on this run's speech: the rest of its replies show on the island unsaid, until his own words
    /// reach the agent again or the run settles (review 5 Nov: the turn's final reply was said right after his Stop).
    stopped: bool,
    /// Which run this is: one more each time a run settles.
    run: u64,
}
/// The run a reply was queued in, and the microphone line that woke or steered it (none: a T3 result, a notice).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RunTag {
    pub(crate) run: u64,
    pub(crate) line: Option<String>,
}
/// The user's words count for `listen_to` this long after they woke the agent (a wake Agent off never delivered goes
/// stale instead of authorising a later run).
pub(crate) const LISTEN_WORDS_NS: u64 = 600_000_000_000;
const OWN_SOUNDS: usize = 8;
/// About two minutes of speech, sentence by sentence.
const PLAYED: usize = 48;
/// How long echo cancellation stays on after Heimdall's voice stops (its echo tail, and the next sentence of a reply
/// that pauses to synthesize).
pub(crate) const ECHO_CANCEL_TAIL: Duration = Duration::from_secs(3);
use crate::pipeline::echo_guard::Played;
impl Default for SpeechPolicy {
    fn default() -> Self {
        let cue_at_ns = Arc::new(AtomicU64::new(0));
        let player = Mutex::new(default_player(cue_at_ns.clone()));
        Self { cue_on: AtomicBool::new(true), speaking_on: AtomicBool::new(true), #[cfg(test)] test_bluetooth: AtomicBool::new(false), cue_at_ns, player, other_line_ns: AtomicU64::new(0), other_voice: Default::default(), call_gone_ns: AtomicU64::new(0), farewell_ns: AtomicU64::new(0), addressed: Default::default(), call_voice_hook: Default::default(), own: Default::default(),
            played: Default::default(), echo_cancel_until: AtomicU64::new(0), echo_cancel_off: AtomicBool::new(false), run_voices: Default::default(),
            held: Default::default(), reply_queued_ns: AtomicU64::new(0), owner_words_ns: AtomicU64::new(0) }
    }
}
impl SpeechPolicy {
    pub(crate) fn set_cue_enabled(&self, on: bool) {
        self.cue_on.store(on, Ordering::Release);
    }
    pub(crate) fn cue_enabled(&self) -> bool {
        self.cue_on.load(Ordering::Acquire)
    }
    /// Settings `speak_replies` (Speaking on): replies play, so the cue may claim Bluetooth headphones too.
    pub(crate) fn set_speaking(&self, on: bool) {
        self.speaking_on.store(on, Ordering::Release);
    }
    /// A spoken wake went to the agent: the cue plays now (when Wake Sound is on) and its window opens.
    pub(crate) fn wake_sent(&self, now_ns: u64) -> bool {
        #[cfg(not(test))]
        let bluetooth = speak::route::default_output_is_bluetooth();
        #[cfg(test)]
        let bluetooth = self.test_bluetooth.load(Ordering::Acquire);
        if let Err(reason) = cue_allowed(self.cue_enabled(), bluetooth, self.speaking_on.load(Ordering::Acquire)) {
            if reason != "wake_sound_off" { cue_log("skipped", reason); }
            return false;
        }
        self.cue_at_ns.store(now_ns.max(1), Ordering::Release);
        let player = self.player.lock().unwrap().clone();
        player();
        true
    }
    /// The cue is sounding (or its tail is) at `at_ns`, host clock.
    pub(crate) fn cue_sounding(&self, at_ns: u64) -> bool {
        let start = self.cue_at_ns.load(Ordering::Acquire);
        start != 0 && at_ns >= start && at_ns - start <= CUE_WINDOW.as_nanos() as u64
    }
    /// An utterance from `start_ns` to `end_ns` (host clock) lies wholly inside the cue's window: it is the cue on the
    /// microphone, not the user.
    pub(crate) fn cue_covers(&self, start_ns: u64, end_ns: u64) -> bool {
        start_ns <= end_ns && ((self.cue_sounding(start_ns) && self.cue_sounding(end_ns))
            || self.own.lock().unwrap().iter().any(|&(from, sounding, to)| from <= start_ns && start_ns <= sounding && end_ns <= to))
    }
    /// Heimdall's own sound reaches the microphone from `start_ns` to `end_ns`: a microphone utterance lying wholly
    /// inside it is that sound, archived only (`cue_covers`).
    pub(crate) fn own_sound(&self, start_ns: u64, end_ns: u64) {
        self.own_sound_until(start_ns, end_ns, end_ns);
    }
    /// An acknowledgement's span (voice_bank.rs): it opens at `start_ns`, sounds until `sounding_ns` (its last sample)
    /// and closes its tail at `end_ns`. Words lying wholly inside it are its echo only if they began while it sounded.
    /// Words that begin after its last sample are the user's by time (review 23 Oct, finding 2: "No, wait" 0.2 s after
    /// "On it." was thrown away); the text guard still catches a late echo of its words (pipeline/echo_guard.rs).
    pub(crate) fn own_sound_until(&self, start_ns: u64, sounding_ns: u64, end_ns: u64) {
        let mut own = self.own.lock().unwrap();
        let sounding = sounding_ns.max(start_ns);
        own.push_back((start_ns, sounding, end_ns.max(sounding)));
        while own.len() > OWN_SOUNDS {
            own.pop_front();
        }
    }
    /// The speaker started playing `text` at `at_ns` (host clock). Through speakers (`speakers`) it is the echo guard's
    /// reference until it ends; through headphones there is no echo and nothing is kept. Either way the sentence before
    /// it has ended.
    pub(crate) fn sentence_started(&self, text: &str, at_ns: u64, speakers: bool) {
        let mut played = self.played.lock().unwrap();
        if let Some(last) = played.back_mut().filter(|p| p.end_ns == u64::MAX) {
            last.end_ns = at_ns.max(last.start_ns);
        }
        if speakers && !text.trim().is_empty() {
            played.push_back(Played { start_ns: at_ns, end_ns: u64::MAX, text: text.into() });
            while played.len() > PLAYED {
                played.pop_front();
            }
        }
        if speakers {
            self.want_echo_cancel(at_ns, ECHO_CANCEL_TAIL);
        }
    }
    /// Heimdall's voice stopped at `at_ns`: the playing sentence ends, and echo cancellation stays a tail longer.
    pub(crate) fn voice_stopped(&self, at_ns: u64) {
        let mut played = self.played.lock().unwrap();
        if let Some(last) = played.back_mut().filter(|p| p.end_ns == u64::MAX) {
            last.end_ns = at_ns.max(last.start_ns);
            drop(played);
            self.want_echo_cancel(at_ns, ECHO_CANCEL_TAIL);
        }
    }
    /// Sentences played through speakers that may overlap `start_ns..end_ns` with their lead and tail.
    pub(crate) fn played_around(&self, start_ns: u64, end_ns: u64) -> Vec<Played> {
        use crate::pipeline::echo_guard::{LEAD_NS, TAIL_NS};
        self.played.lock().unwrap().iter()
            .filter(|p| p.start_ns <= end_ns.saturating_add(LEAD_NS) && p.end_ns.saturating_add(TAIL_NS) >= start_ns)
            .cloned().collect()
    }
    /// Settings `echo_cancel_off` is not set.
    pub(crate) fn echo_cancel_on(&self) -> bool {
        !self.echo_cancel_off.load(Ordering::Acquire)
    }
    pub(crate) fn set_echo_cancel_off(&self, off: bool) {
        self.echo_cancel_off.store(off, Ordering::Release);
    }
    /// Echo cancellation is wanted from now until `at_ns + for_`.
    pub(crate) fn want_echo_cancel(&self, at_ns: u64, for_: Duration) {
        self.echo_cancel_until.fetch_max(at_ns.saturating_add(for_.as_nanos() as u64), Ordering::AcqRel);
    }
    /// Whether echo cancellation is wanted at `now_ns` by what played: a sentence is playing through speakers, or one
    /// ended less than `ECHO_CANCEL_TAIL` ago. (Speech queued or synthesizing also wants it: bridge/echo_cancel.rs.)
    pub(crate) fn echo_cancel_wanted(&self, now_ns: u64) -> bool {
        !self.echo_cancel_off.load(Ordering::Acquire)
            && (self.played.lock().unwrap().back().is_some_and(|p| p.end_ns == u64::MAX)
                || now_ns < self.echo_cancel_until.load(Ordering::Acquire))
    }
    /// A call line with words ended at `end_ns` (host clock).
    pub(crate) fn other_line(&self, end_ns: u64) {
        self.other_line_ns.fetch_max(end_ns, Ordering::AcqRel);
    }
    /// The user is in a conversation at `now_ns` (host clock): a call line ended at most `CONVERSATION_NS` before, or another
    /// participant is talking on a call source now; no goodbye closed it since, and a call source is still running.
    pub(crate) fn in_conversation(&self, now_ns: u64) -> bool {
        let last = self.other_line_ns.load(Ordering::Acquire);
        let farewell = self.farewell_ns.load(Ordering::Acquire);
        let gone = self.call_gone_ns.load(Ordering::Acquire);
        // The last call source stopped a moment ago and nothing was heard on one since: the call is over.
        if gone != 0 && gone >= last && now_ns.saturating_sub(gone) >= CALL_SOURCE_GONE.as_nanos() as u64 {
            return false;
        }
        let heard = last != 0 && now_ns >= last
            && decide::send::call_active_within(Some(last), (farewell != 0).then_some(farewell), now_ns, CONVERSATION_NS);
        heard || self.other_talking(now_ns)
    }
    /// Another participant is talking on a call source at `now_ns`: a voice begun after any goodbye (the goodbye's own
    /// words end before it is known), open at most `OPEN_VOICE_MAX`.
    fn other_talking(&self, now_ns: u64) -> bool {
        let farewell = self.farewell_ns.load(Ordering::Acquire);
        self.other_voice.lock().unwrap().values().any(|&from| {
            from > farewell.saturating_add(decide::send::FAREWELL_SLACK_NS) && now_ns >= from && now_ns - from <= OPEN_VOICE_MAX.as_nanos() as u64
        })
    }
    /// Another participant's voice on call source `source` started at `at_ns` (`Some`) or ended (`None`): the call is
    /// live while it is open (a long turn whose words are not in yet).
    /// The app hears of it (`set_call_voice_hook`) after the lock is released: a voice begun before a goodbye is not
    /// the call going on and is not reported.
    pub(crate) fn other_voice(&self, source: &str, at_ns: Option<u64>) {
        let report = {
            let mut open = self.other_voice.lock().unwrap();
            match at_ns {
                Some(at) => { open.insert(source.to_owned(), at.max(1)); (at.max(1) > self.farewell_ns.load(Ordering::Acquire).saturating_add(decide::send::FAREWELL_SLACK_NS)).then_some(Some(at.max(1))) }
                None => open.remove(source).map(|_| None),
            }
        };
        if let Some(at) = report { self.call_voice(source, at); }
    }
    /// When the voice open on call source `source` began (host clock), if one is.
    pub(crate) fn voice_open_since(&self, source: &str) -> Option<u64> {
        self.other_voice.lock().unwrap().get(source).copied()
    }
    /// The app's reaction to a voice on a call source: the reply hold and the guest barge-in (bridge/speech.rs).
    pub(crate) fn set_call_voice_hook(&self, hook: Option<CallVoiceHook>) {
        *self.call_voice_hook.lock().unwrap() = hook;
    }
    fn call_voice(&self, source: &str, at_ns: Option<u64>) {
        let hook = self.call_voice_hook.lock().unwrap().clone();
        if let Some(hook) = hook { hook(source, at_ns); }
    }
    /// Heimdall's voice level for a line queued at `now_ns`: lower while the user is in a call (`CALL_VOICE_GAIN`).
    pub(crate) fn voice_gain(&self, now_ns: u64) -> f32 {
        if self.in_conversation(now_ns) { CALL_VOICE_GAIN } else { 1. }
    }
    /// A call source started: a call can be running.
    pub(crate) fn call_source_added(&self) {
        self.call_gone_ns.store(0, Ordering::Release);
    }
    /// Call source `source` was removed at `at_ns`; `stopped`: its app stopped playing (not a stalled tap, App Audio
    /// switched off or sleep, after which the call may go on unheard); `any_left`: another call source is still
    /// running. Stopped with none left, the call ends `CALL_SOURCE_GONE` later unless one comes back.
    pub(crate) fn call_source_removed(&self, source: &str, stopped: bool, any_left: bool, at_ns: u64) {
        if self.other_voice.lock().unwrap().remove(source).is_some() { self.call_voice(source, None); }
        if stopped && !any_left {
            self.call_gone_ns.store(at_ns.max(1), Ordering::Release);
        }
    }
    /// A goodbye in the call ended at `end_ns`: replies are spoken again (29 Oct 22:28, held "in a call" after he left).
    pub(crate) fn farewell(&self, end_ns: u64) {
        self.farewell_ns.fetch_max(end_ns, Ordering::AcqRel);
    }
    /// A call line ended at most `window_ns` before `now_ns` (host clock).
    pub(crate) fn call_heard_within(&self, now_ns: u64, window_ns: u64) -> bool {
        let last = self.other_line_ns.load(Ordering::Acquire);
        last != 0 && now_ns.checked_sub(last).is_some_and(|ago| ago <= window_ns)
    }
    /// The latest own-sound span (tests).
    #[cfg(test)]
    pub(crate) fn last_own_sound(&self) -> Option<(u64, u64)> {
        self.own.lock().unwrap().back().map(|&(from, _, to)| (from, to))
    }
    /// Host clock of the last cue's play request (0: none).
    #[cfg(test)]
    pub(crate) fn cue_at(&self) -> u64 {
        self.cue_at_ns.load(Ordering::Acquire)
    }
    /// The last call line ended at `end_ns`, earlier ones included (tests: time passing).
    #[cfg(test)]
    pub(crate) fn test_last_line(&self, end_ns: u64) {
        self.other_line_ns.store(end_ns, Ordering::Release);
    }
    /// The user addressed Heimdall by name and that wake went out: the run it starts (or steers) may answer aloud.
    pub(crate) fn addressed_wake(&self) {
        self.addressed.store(true, Ordering::Release);
    }
    /// The current run may answer aloud in a conversation (the user addressed Heimdall by name).
    pub(crate) fn addressed_now(&self) -> bool {
        self.addressed.load(Ordering::Acquire)
    }
    /// The flag itself, for the skipped reply's retry: it wakes the agent from its own thread after the addressed run
    /// settled, and its answer may be spoken in the call (bug notes 2026-11-04T20-52-24, 2026-11-05T00-14-40).
    pub(crate) fn addressed_flag(&self) -> Arc<AtomicBool> {
        self.addressed.clone()
    }
    /// The user's latest words that woke the current run, if theirs did (the Stop button's evidence).
    pub(crate) fn run_words(&self) -> Option<String> {
        self.run_voices.lock().unwrap().owner.last().map(|(_, w)| w.clone())
    }
    /// The user's microphone line `id` woke or steered the current run (`Pipeline::dispatch_pending`).
    pub(crate) fn line_woke(&self, id: &str) {
        self.run_voices.lock().unwrap().line = Some(id.to_owned());
    }
    /// The current run (a number that changes each time a run settles) and the microphone line that woke or steered
    /// it: what a reply queued now belongs to (`Tools::run_speech`).
    pub(crate) fn run_tag(&self) -> RunTag {
        let v = self.run_voices.lock().unwrap();
        RunTag { run: v.run, line: v.line.clone() }
    }
    /// The user pressed Stop on speech of run `run`: if it is still the current run, its later replies are shown unsaid.
    pub(crate) fn stop_run(&self, run: u64) {
        let mut v = self.run_voices.lock().unwrap();
        if v.run == run { v.stopped = true; }
    }
    /// The user stopped the current run's speech: its later replies are shown, not said.
    pub(crate) fn run_stopped(&self) -> bool {
        self.run_voices.lock().unwrap().stopped
    }
    /// The agent's run settled (or was stopped, or the agent started): the next run is not addressed until told.
    pub(crate) fn run_settled(&self) {
        self.addressed.store(false, Ordering::Release);
        let mut v = self.run_voices.lock().unwrap();
        *v = RunVoices { run: v.run.wrapping_add(1), ..RunVoices::default() };
    }
    /// The user's own words (his microphone lines or a typed message) went to the agent at `at_ns`.
    pub(crate) fn owner_woke(&self, words: &str, at_ns: u64) {
        self.owner_words_ns.fetch_max(at_ns, Ordering::AcqRel);
        let mut v = self.run_voices.lock().unwrap();
        // They spoke to Heimdall again after a Stop: the answer to these words is said.
        v.stopped = false;
        v.owner.push((at_ns, words.chars().take(2000).collect()));
        let drop = v.owner.len().saturating_sub(16);
        v.owner.drain(..drop);
    }
    /// A line that is not the user's (a guest's) woke or steered the agent.
    pub(crate) fn guest_woke(&self) {
        self.run_voices.lock().unwrap().guest = true;
    }
    /// Whether `listen_to` may change `target`'s listening now: only in a run the user's own microphone or typed words
    /// woke, never one a guest's words woke or steered; turning it on only when those words ask to listen to
    /// `target` by name or as him, her or them ("listen to Vim", "posłuchaj go"), not to stop. Turning it off, and
    /// joining another voice to a listened guest (`target` None), need only his wake. The error is the tool's answer.
    pub(crate) fn listen_change(&self, on: Option<&str>, now_ns: u64) -> Result<(), &'static str> {
        let v = self.run_voices.lock().unwrap();
        if v.guest {
            return Err("NOT_ALLOWED: a guest's words woke or steered this turn, so it cannot change who you listen to. Finish this turn; User can say it again after it.");
        }
        let fresh: Vec<&str> = v.owner.iter().filter(|(at, _)| now_ns.saturating_sub(*at) <= LISTEN_WORDS_NS).map(|(_, w)| w.as_str()).collect();
        if fresh.is_empty() {
            return Err("NOT_ALLOWED: only User's own words can change who you listen to, and none woke this turn.");
        }
        if let Some(target) = on {
            if !fresh.iter().any(|w| asks_to_listen(w, target)) {
                return Err("NOT_ALLOWED: User's words in this turn did not ask you to listen to them. Act on a guest only when he says so himself (\"listen to Vim\").");
            }
        }
        Ok(())
    }
    /// Speech is held at `now_ns` (host clock): the user is in a conversation and the current run was not woken by him
    /// addressing Heimdall by name.
    pub(crate) fn speech_held(&self, now_ns: u64) -> bool {
        self.in_conversation(now_ns) && !self.addressed.load(Ordering::Acquire)
    }
    /// A reply was held at `at_ns` (host clock) because the user is in a conversation: kept to be said once it is over.
    pub(crate) fn hold_line(&self, text: &str, language: &str, at_ns: u64, permit: &agent::delivery::DeliveryPermit) {
        let mut held = self.held.lock().unwrap();
        held.push_back(HeldLine { text: text.into(), language: language.into(), held_ns: at_ns, permit: permit.clone() });
        while held.len() > HELD_LINES {
            if let Some(oldest) = held.pop_front() {
                held_log("dropped: more than four held", at_ns.saturating_sub(oldest.held_ns));
            }
        }
    }
    /// Stop or the user talking over Heimdall: lines held for after the call are not said. (A typed message keeps them:
    /// `Tools::stop_speech`.)
    pub(crate) fn drop_held(&self, reason: &str, now_ns: u64) {
        for line in self.held.lock().unwrap().drain(..) {
            held_log(reason, now_ns.saturating_sub(line.held_ns));
        }
    }
    /// A reply was queued for speech at `at_ns`: lines held before it are superseded.
    pub(crate) fn reply_queued(&self, at_ns: u64) {
        self.reply_queued_ns.fetch_max(at_ns, Ordering::AcqRel);
    }
    /// Lines are held for after the conversation (one lock, nothing else: checked on every capture event).
    pub(crate) fn has_held(&self) -> bool {
        !self.held.lock().unwrap().is_empty()
    }
    /// The held lines to say now, taken once, oldest first, each with whether the user asked for it (`Due::addressed`).
    /// Stale and superseded lines are dropped first, each with a log line. Of the lines held before the user's latest words
    /// reached the agent, the newest is said now even in a conversation (he is talking to Heimdall) and the older are
    /// dropped; the rest wait for the conversation to end. Nothing while `may_speak` is false (he is talking, Speaking
    /// or the agent is off, Heimdall is already speaking).
    pub(crate) fn due_held(&self, now_ns: u64, may_speak: bool) -> Vec<Due> {
        let reply = self.reply_queued_ns.load(Ordering::Acquire);
        let words = self.owner_words_ns.load(Ordering::Acquire);
        let mut held = self.held.lock().unwrap();
        held.retain(|line| {
            let age = now_ns.saturating_sub(line.held_ns);
            let reason = if age > HELD_FOR.as_nanos() as u64 { Some("dropped: stale") }
                else if reply > line.held_ns { Some("dropped: a newer reply was spoken") }
                else if line.permit.check().is_err() { Some("dropped: Agent or Heimdall was off since") }
                else { None };
            if let Some(reason) = reason { held_log(reason, age); }
            reason.is_none()
        });
        // His words came after these: only the newest is still worth saying.
        let asked = held.iter().filter(|line| words > line.held_ns).count();
        for line in held.drain(..asked.saturating_sub(1)) {
            held_log("dropped: a newer held line is said for User's words", now_ns.saturating_sub(line.held_ns));
        }
        if held.is_empty() || !may_speak {
            return vec![];
        }
        if !self.in_conversation(now_ns) {
            return held.drain(..).map(|line| Due { addressed: words > line.held_ns, line }).collect();
        }
        // Not over the other person: it waits for their voice to end.
        match held.front() {
            Some(first) if words > first.held_ns && !self.other_talking(now_ns) => held.pop_front().map(|line| Due { line, addressed: true }).into_iter().collect(),
            _ => vec![],
        }
    }
    /// Tests: every held line was held `by_ns` earlier.
    #[cfg(test)]
    pub(crate) fn test_age_held(&self, by_ns: u64) {
        for line in self.held.lock().unwrap().iter_mut() {
            line.held_ns = line.held_ns.saturating_sub(by_ns);
        }
    }
    /// Tests: the lines waiting for after the conversation.
    #[cfg(test)]
    pub(crate) fn test_held(&self) -> Vec<HeldLine> {
        self.held.lock().unwrap().iter().cloned().collect()
    }
    /// Tests: the default output is Bluetooth headphones.
    #[cfg(test)]
    pub(crate) fn test_set_bluetooth(&self, on: bool) {
        self.test_bluetooth.store(on, Ordering::Release);
    }
    /// Replays and tests swap the player (a replay never sounds on the user's speakers).
    pub(crate) fn set_player(&self, player: CuePlayer) {
        *self.player.lock().unwrap() = player;
    }
}

/// One heimdall.log line for a held line's fate: "replayed after the call", or why it was dropped.
pub(crate) fn held_log(what: &str, age_ns: u64) {
    eprintln!("speech: held line {what} ({} s later)", age_ns / 1_000_000_000);
}

/// Whether a wake's cue plays, or why not.
/// voice (night 1022): any sound started on Bluetooth headphones makes macOS claim them back from the user's iPhone (22 Oct
/// 02:54: his AirPods left a video 50 ms after a wake's Tink, with Speaking off). Skipping the cue on every Bluetooth
/// output silenced it whenever his AirPods were the Mac's output (26 Oct 16:03: "the wake sound doesn't play"; every
/// wake from 15:49 to 16:12 went out on AirPods). With Speaking on, the reply claims them the same way a moment later,
/// so the cue plays there too; with Speaking off, the cue would be Heimdall's only sound on them, and it stays quiet.
fn cue_allowed(wake_sound: bool, bluetooth: bool, speaking: bool) -> Result<(), &'static str> {
    if !wake_sound {
        Err("wake_sound_off")
    } else if bluetooth && !speaking {
        Err("bluetooth_output_with_speaking_off")
    } else {
        Ok(())
    }
}
/// At most one wake-sound line per this period for the same outcome; the rest are counted into the next line.
const CUE_LOG_PERIOD: Duration = Duration::from_secs(60);
/// One heimdall.log line for the cue: `outcome` ("played", "skipped", "failed") and its detail. Rate-limited.
fn cue_log(outcome: &'static str, detail: &str) {
    static LAST: Mutex<Option<(std::time::Instant, &'static str, u32)>> = Mutex::new(None);
    let now = std::time::Instant::now();
    let mut last = LAST.lock().unwrap();
    let more = match *last {
        Some((at, was, n)) if was == outcome && now.duration_since(at) < CUE_LOG_PERIOD => {
            *last = Some((at, was, n + 1));
            return;
        }
        Some((_, was, n)) if n > 0 && was == outcome => format!(" (+{n} more since the last line)"),
        Some((_, was, n)) if n > 0 => format!(" (+{n} more {was} since the last line)"),
        _ => String::new(),
    };
    *last = Some((now, outcome, 0));
    drop(last);
    eprintln!("Wake sound: {outcome}: {detail}{more}");
}
/// The cue actually started playing at `at_ns` (host clock): its window starts there. Never moves the window back (a
/// late main-queue run of an older request).
fn cue_started(cue_at_ns: &AtomicU64, at_ns: u64) {
    cue_at_ns.fetch_max(at_ns.max(1), Ordering::AcqRel);
}
#[cfg(not(test))]
fn default_player(cue_at_ns: Arc<AtomicU64>) -> CuePlayer {
    Arc::new(move || play_tink(cue_at_ns.clone()))
}
/// Tests never make a sound unless they install a player.
#[cfg(test)]
fn default_player(_: Arc<AtomicU64>) -> CuePlayer {
    Arc::new(|| {})
}
/// Tink at `CUE_VOLUME`, on the main queue (AppKit), without waiting; the cue's window starts when it actually
/// plays. A cue still sounding is not restarted.
#[cfg(not(test))]
fn play_tink(cue_at_ns: Arc<AtomicU64>) {
    dispatch2::DispatchQueue::main().exec_async(move || {
        let Some(sound) = objc2_app_kit::NSSound::soundNamed(&objc2_foundation::NSString::from_str(CUE_SOUND)) else {
            cue_log("failed", &format!("no system sound {CUE_SOUND}"));
            return;
        };
        if sound.isPlaying() { return; }
        sound.setVolume(CUE_VOLUME);
        cue_started(&cue_at_ns, capture::host_now_ns());
        if sound.play() {
            let route = speak::route::default_output().map_or_else(|| "unknown output".into(), |r| format!("{} ({})", r.name, r.transport.label()));
            cue_log("played", &format!("{CUE_SOUND} on {route}"));
        } else {
            cue_log("failed", &format!("NSSound refused to play {CUE_SOUND}"));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u64 = 1_000_000_000;

    #[test]
    fn echo_ledger_keeps_only_speaker_sentences_and_wants_cancellation_while_they_play() {
        let p = SpeechPolicy::default();
        assert!(!p.echo_cancel_wanted(100 * S));
        p.sentence_started("Through headphones.", 100 * S, false);
        assert!(p.played_around(99 * S, 102 * S).is_empty(), "headphones: no echo, nothing kept");
        assert!(!p.echo_cancel_wanted(100 * S), "and no echo cancellation");
        p.sentence_started("First sentence.", 110 * S, true);
        p.sentence_started("Second sentence.", 112 * S, true);
        let played = p.played_around(111 * S, 113 * S);
        assert_eq!(played.len(), 2);
        assert_eq!((played[0].end_ns, played[1].end_ns), (112 * S, u64::MAX), "the next start ends the one before");
        assert!(p.echo_cancel_wanted(500 * S), "wanted while a sentence plays");
        p.voice_stopped(114 * S);
        assert!(p.echo_cancel_wanted(114 * S + ECHO_CANCEL_TAIL.as_nanos() as u64 - 1));
        assert!(!p.echo_cancel_wanted(114 * S + ECHO_CANCEL_TAIL.as_nanos() as u64), "off a tail after the voice stops");
        assert!(p.played_around(200 * S, 201 * S).is_empty(), "long after: not echo");
        p.want_echo_cancel(300 * S, Duration::from_secs(2));
        assert!(p.echo_cancel_wanted(301 * S));
        p.set_echo_cancel_off(true);
        assert!(!p.echo_cancel_wanted(301 * S), "settings echo_cancel_off");
    }
    #[test]
    fn cue_plays_on_bluetooth_only_while_speaking_is_on() {
        assert_eq!(cue_allowed(true, false, false), Ok(()));
        assert_eq!(cue_allowed(true, true, true), Ok(()), "AirPods with Speaking on (26 Oct 16:03)");
        assert_eq!(cue_allowed(true, true, false), Err("bluetooth_output_with_speaking_off"), "22 Oct 02:54");
        assert_eq!(cue_allowed(false, false, true), Err("wake_sound_off"));
        let p = SpeechPolicy::default();
        p.test_set_bluetooth(true);
        p.set_speaking(false);
        assert!(!p.wake_sent(100 * S) && p.cue_at() == 0, "no cue window without the cue");
        p.set_speaking(true);
        assert!(p.wake_sent(100 * S));
    }

    #[test]
    fn cue_window_covers_only_what_lies_wholly_inside_it() {
        let p = SpeechPolicy::default();
        let played = Arc::new(AtomicU64::new(0));
        let count = played.clone();
        p.set_player(Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
        assert!(!p.cue_sounding(100 * S), "no cue yet");
        assert!(p.wake_sent(100 * S));
        assert_eq!(played.load(Ordering::SeqCst), 1);
        let w = CUE_WINDOW.as_nanos() as u64;
        assert!(p.cue_covers(100 * S, 100 * S + w), "both edges are inside");
        assert!(p.cue_covers(100 * S + 200_000_000, 100 * S + 700_000_000));
        assert!(!p.cue_covers(100 * S - 1, 100 * S + 500_000_000), "began before the cue: User");
        assert!(!p.cue_covers(100 * S + 500_000_000, 100 * S + w + 1), "ran past the window: User");
        assert!(!p.cue_sounding(100 * S + w + 1));
        // Off: no sound, no window.
        p.set_cue_enabled(false);
        assert!(!p.wake_sent(200 * S));
        assert_eq!(played.load(Ordering::SeqCst), 1);
        assert!(!p.cue_sounding(200 * S));
    }

    /// Night 3 review: Tink plays on the main queue, which may run the play request later; the window starts when the
    /// cue actually starts, so speech beginning between the request and the playback is the user's.
    #[test]
    fn the_cue_window_starts_when_the_cue_actually_plays() {
        let p = SpeechPolicy::default();
        assert!(p.wake_sent(100 * S));
        let w = CUE_WINDOW.as_nanos() as u64;
        // The main queue runs the request 300 ms late.
        cue_started(&p.cue_at_ns, 100 * S + 300_000_000);
        assert_eq!(p.cue_at(), 100 * S + 300_000_000);
        assert!(!p.cue_covers(100 * S + 100_000_000, 100 * S + 600_000_000), "began before the cue played: User");
        assert!(p.cue_covers(100 * S + 400_000_000, 100 * S + 300_000_000 + w), "the cue's own sound, to the window's end");
        // A stale run of an earlier request never moves the window back.
        cue_started(&p.cue_at_ns, 99 * S);
        assert_eq!(p.cue_at(), 100 * S + 300_000_000);
    }

    /// Only his voice changes who Heimdall listens to; a guest can never grant it.
    #[test]
    fn only_owners_own_words_change_listening_and_never_in_a_run_a_guest_woke() {
        let p = SpeechPolicy::default();
        assert!(p.listen_change(Some("Vim"), 10 * S).is_err(), "no wake at all (a notice, a task's reply)");
        p.owner_woke("Heimdall, what time is it?", 10 * S);
        assert!(p.listen_change(Some("Vim"), 11 * S).is_err(), "his words did not ask for listening");
        assert!(p.listen_change(None, 11 * S).is_ok(), "turning it off needs only his wake");
        p.owner_woke("Hey Heimdall, listen to Vim right now.", 12 * S);
        assert!(p.listen_change(Some("Vim"), 13 * S).is_ok());
        assert!(p.listen_change(Some("Dave"), 13 * S).is_err(), "he named Vim, not Dave");
        assert!(p.listen_change(Some("Vim"), 12 * S + LISTEN_WORDS_NS + 1).is_err(), "stale words authorise nothing");
        p.guest_woke();
        assert!(p.listen_change(Some("Vim"), 13 * S).is_err() && p.listen_change(None, 13 * S).is_err(), "a guest steered the run");
        p.run_settled();
        assert!(p.listen_change(None, 14 * S).is_err(), "the next run starts with nothing");
    }
    #[test]
    fn asking_to_listen_needs_a_positive_listen_and_the_guest() {
        for (words, target, asks) in [
            ("Hey Heimdall, listen to Vim right now.", "Vim", true),
            ("Hey Heimdall, listen to him right now.", "Vim", true),
            ("Heimdall, posłuchaj go.", "Vim", true),
            ("Heimdall, listen to angel sister.", "angel sister", true),
            ("Heimdall, stop listening to Vim.", "Vim", false),
            ("Heimdall, don't listen to him.", "Vim", false),
            ("Heimdall, nie słuchaj go.", "Vim", false),
            ("Heimdall, gdzie są moje słuchawki?", "Vim", false),
            ("Heimdall, listen to Vim.", "Dave", false),
            ("Vim is cool.", "Vim", false),
            ("Heimdall, listen to Vim and angel sister.", "angel sister", true),
            ("Heimdall, listen to Vim, let's go.", "Dave", false),
            ("Heimdall, listen to Vim, not Dave.", "Dave", false),
            ("Heimdall, listen to him.", "Dave", true),
            ("Ja nie wiem, Heimdall, listen to Vim.", "Dave", false),
        ] {
            assert_eq!(asks_to_listen(words, target), asks, "{words} / {target}");
        }
    }

    #[test]
    fn a_conversation_lasts_45_seconds_past_the_last_call_line() {
        let p = SpeechPolicy::default();
        assert!(!p.in_conversation(500 * S), "no call line yet");
        p.other_line(300 * S);
        assert!(p.in_conversation(300 * S));
        assert!(p.in_conversation(300 * S + CONVERSATION_NS), "the boundary itself is still the conversation");
        assert!(!p.in_conversation(300 * S + CONVERSATION_NS + 1), "speech resumes after it");
        // An older line arriving late (diarization) never moves the end back.
        p.other_line(250 * S);
        assert!(p.in_conversation(300 * S + CONVERSATION_NS));
        // A clock before the line (a replay's synthetic time) is not a conversation.
        assert!(!p.in_conversation(299 * S));
        assert_eq!(CONVERSATION_NS, 45 * S);
    }

    /// Bug 2026-11-04T17-48-24: Angel's last line ended 17:46:29 and the user talked to Heimdall from 17:46:50; Heimdall
    /// held every reply until 17:48:29. Call lines stop, he speaks 60 s later: not in a call.
    #[test]
    fn the_call_ends_within_the_window_of_the_last_call_line_not_two_minutes() {
        let p = SpeechPolicy::default();
        p.other_line(1_000 * S);
        assert!(p.speech_held(1_010 * S), "the call is live");
        assert!(!p.in_conversation(1_060 * S) && !p.speech_held(1_060 * S), "60 s after the last call line");
        // A goodbye in the call ends it at once; a new call line after it reopens it.
        p.other_line(2_000 * S);
        p.farewell(2_000 * S);
        assert!(!p.in_conversation(2_001 * S));
        p.other_line(2_010 * S);
        assert!(p.in_conversation(2_011 * S));
    }

    /// Live evidence: another participant talking on a call source keeps the call live past the window while the words
    /// are not in yet; the end of that voice leaves only the window.
    #[test]
    fn an_open_voice_on_a_call_source_keeps_the_call_live() {
        let p = SpeechPolicy::default();
        p.other_line(100 * S);
        p.other_voice("process:discord", Some(140 * S));
        assert!(p.in_conversation(100 * S + CONVERSATION_NS + 20 * S), "a long turn whose words are not in yet");
        assert!(!p.in_conversation(140 * S + OPEN_VOICE_MAX.as_nanos() as u64 + 1), "a voice that never ended stops counting");
        p.other_voice("process:discord", None);
        assert!(!p.in_conversation(100 * S + CONVERSATION_NS + 20 * S), "the voice ended");
        // A voice begun before a goodbye is not the call going on; one begun after it is.
        p.farewell(300 * S);
        p.other_voice("process:discord", Some(299 * S));
        assert!(!p.in_conversation(301 * S));
        p.other_voice("process:discord", Some(310 * S));
        assert!(p.in_conversation(311 * S));
    }

    /// The call source stopping ends the call a few seconds later; a route change that brings it back does not.
    #[test]
    fn the_last_call_source_stopping_ends_the_call() {
        let gone = CALL_SOURCE_GONE.as_nanos() as u64;
        let p = SpeechPolicy::default();
        p.other_line(100 * S);
        p.other_voice("process:a", Some(101 * S));
        p.call_source_removed("process:b", true, true, 102 * S);
        assert!(p.in_conversation(102 * S + gone), "another call source is still running");
        p.call_source_removed("process:a", false, false, 102 * S);
        assert!(p.in_conversation(102 * S + gone), "a stalled tap or App Audio off: the call may go on unheard");
        p.call_source_removed("process:a", true, false, 102 * S);
        assert!(p.in_conversation(102 * S + gone - 1), "a route change returns the source within ~100 ms");
        assert!(!p.in_conversation(102 * S + gone), "the call source stopped");
        p.call_source_added();
        assert!(p.in_conversation(102 * S + gone), "it came back: the window still runs");
        // A call line heard after the source stopped is the call again.
        p.call_source_removed("process:a", true, false, 110 * S);
        p.other_line(111 * S);
        assert!(p.in_conversation(111 * S + gone));
    }

    fn permit() -> agent::delivery::DeliveryPermit {
        Arc::new(agent::delivery::DeliveryGate::new(true)).permit().unwrap()
    }
    fn texts(due: &[Due]) -> Vec<(&str, bool)> {
        due.iter().map(|d| (d.line.text.as_str(), d.addressed)).collect()
    }

    /// Bug 2026-11-04T17-48-24: each held reply was dropped unheard when the user's next words reached the agent. Now the
    /// newest held before his words is said as soon as he is quiet, in a call or not; older ones are dropped (each was
    /// shown on the island when held). A line with nothing of his after it still waits for the call to end.
    #[test]
    fn his_words_after_a_held_line_say_the_newest_even_in_a_call() {
        let p = SpeechPolicy::default();
        let pm = permit();
        p.other_line(100 * S);
        p.hold_line("Everything's fine.", "en", 101 * S, &pm);
        p.hold_line("Still the 08:14 build.", "en", 102 * S, &pm);
        assert!(p.due_held(103 * S, true).is_empty(), "nothing of his yet: they wait for the call to end");
        // Call talk that never reached the agent changes nothing.
        p.other_line(104 * S);
        assert!(p.due_held(105 * S, true).is_empty());
        assert_eq!(p.test_held().len(), 2);
        // He talks to Heimdall.
        p.owner_woke("Heimdall, hello?", 106 * S);
        assert!(p.due_held(106 * S, false).is_empty(), "he is still talking");
        assert_eq!(p.test_held().len(), 1, "the older line is dropped");
        p.hold_line("Website is open.", "en", 107 * S, &pm);
        assert!(p.in_conversation(108 * S));
        p.other_voice("process:discord", Some(108 * S));
        assert!(p.due_held(108 * S, true).is_empty(), "not over the other person");
        p.other_voice("process:discord", None);
        assert_eq!(texts(&p.due_held(108 * S, true)), [("Still the 08:14 build.", true)], "said in the call");
        assert_eq!(p.test_held().iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["Website is open."], "held after his words: it waits");
        // After the call, the rest is said as usual.
        assert_eq!(texts(&p.due_held(104 * S + CONVERSATION_NS + 1, true)), [("Website is open.", false)]);
        // A newer reply queued after his words supersedes the held one: that reply is what he hears.
        p.other_line(300 * S);
        p.hold_line("Old reply.", "en", 301 * S, &pm);
        p.owner_woke("what about the deploy", 302 * S);
        p.reply_queued(303 * S);
        assert!(p.due_held(304 * S, true).is_empty() && p.test_held().is_empty());
    }

    #[test]
    fn an_addressed_run_may_speak_in_a_conversation_until_it_settles() {
        let p = SpeechPolicy::default();
        p.other_line(300 * S);
        assert!(p.speech_held(310 * S));
        p.addressed_wake();
        assert!(!p.speech_held(310 * S), "User addressed Heimdall by name");
        p.run_settled();
        assert!(p.speech_held(320 * S), "the next run is held again");
        p.addressed_wake();
        assert!(!p.speech_held(320 * S + CONVERSATION_NS + 1));
    }

    /// Bug note 2026-11-05T02-32-37: a voice line five minutes late has no point. A held line is said only within a
    /// minute of being held, in a quiet gap of the call (his words after it) or after the call; older, it stays the
    /// text it was shown as (held lines were said 136-472 s late on 4-5 Nov).
    #[test]
    fn a_held_line_is_said_only_within_a_minute_and_then_stays_text() {
        assert_eq!(HELD_FOR, Duration::from_secs(60));
        let pm = permit();
        // After the call: a line held 30 s before is said, one held 61 s before is dropped stale.
        let p = SpeechPolicy::default();
        p.other_line(100 * S);
        p.hold_line("Fresh.", "en", 120 * S, &pm);
        assert!(p.due_held(140 * S, true).is_empty(), "the call is live and he said nothing to Heimdall");
        assert_eq!(texts(&p.due_held(150 * S, true)), [("Fresh.", false)], "after the call, 30 s old");
        p.other_line(200 * S);
        p.hold_line("Stale.", "en", 210 * S, &pm);
        assert!(p.due_held(200 * S + CONVERSATION_NS + 26 * S, true).is_empty(), "61 s old when the call ended: text only");
        assert!(p.test_held().is_empty(), "and never later");
        // In the call: his words after it, and he paused while a guest talked past the minute: text only.
        p.other_line(300 * S);
        p.hold_line("Your build is green.", "en", 301 * S, &pm);
        p.owner_woke("Heimdall, is it green?", 302 * S);
        p.other_voice("process:discord", Some(303 * S));
        assert!(p.due_held(340 * S, true).is_empty(), "a guest is talking: no gap yet");
        p.other_voice("process:discord", None);
        assert!(p.due_held(362 * S, true).is_empty() && p.test_held().is_empty(), "the gap came 61 s after: too late");
    }

    /// Bug notes 2026-11-04T20-52-24, 2026-11-05T00-14-40: the skipped reply's retry wakes the agent from its own
    /// thread after the addressed run settled; through the shared flag its answer may be spoken in the call.
    #[test]
    fn the_retry_sets_the_shared_address_flag() {
        let p = SpeechPolicy::default();
        p.other_line(100 * S);
        p.addressed_wake();
        p.run_settled();
        assert!(p.speech_held(101 * S) && !p.addressed_now(), "the addressed run settled before the retry");
        let flag = p.addressed_flag();
        std::thread::spawn(move || flag.store(true, Ordering::Release)).join().unwrap();
        assert!(!p.speech_held(101 * S) && p.addressed_now(), "the retry's answer may be spoken");
    }

    /// Bug note 2026-11-05T02-32-17: a voice opening or ending on a call source reaches the app (the reply hold and
    /// the guest barge-in), outside the lock; a voice begun before a goodbye does not; a removed source ends its voice.
    #[test]
    fn call_voices_reach_the_app() {
        let p = SpeechPolicy::default();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<u64>)>::new()));
        let log = seen.clone();
        p.set_call_voice_hook(Some(Arc::new(move |source: &str, at: Option<u64>| log.lock().unwrap().push((source.into(), at)))));
        p.other_voice("process:discord", Some(10 * S));
        assert_eq!(p.voice_open_since("process:discord"), Some(10 * S));
        p.other_voice("process:discord", None);
        p.other_voice("process:discord", None);
        p.farewell(20 * S);
        p.other_voice("process:discord", Some(19 * S));
        p.call_source_removed("process:discord", true, false, 21 * S);
        p.call_source_removed("process:discord", true, false, 22 * S);
        assert_eq!(*seen.lock().unwrap(), [("process:discord".into(), Some(10 * S)), ("process:discord".into(), None), ("process:discord".into(), None)],
            "one end per voice; the voice begun before the goodbye only ends");
        assert_eq!(p.voice_gain(23 * S), 1., "no call: full voice");
        p.other_line(30 * S);
        assert_eq!(p.voice_gain(31 * S), CALL_VOICE_GAIN, "in a call: quieter (bug note 2026-11-05T00-10-37)");
    }
}
