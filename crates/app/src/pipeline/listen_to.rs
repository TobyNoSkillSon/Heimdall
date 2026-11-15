//! Listening to guests ("Hey Heimdall, listen to Vim right now … I should be also able to go into
//! Speakers and … have a toggle whether you are listening to them or not … I can tell you, hey, listen to him right
//! now, or don't listen to him").
//!
//! - Each named voice has a listen flag in `speakers.json` (`Enrollment::listening`), off by default. It stays on until
//!   The user turns it off, by the Speakers window's switch or the agent's `listen_to`: the switch always shows the truth,
//!   with no expiry hidden behind it. The user is always listened to (his microphone) and Heimdall never.
//! - A listened guest's call line that addresses Heimdall by name (`decide::send::addressed_by_name`) wakes the agent
//!   at once (`Trigger::Guest`), as the user's own address does. The rule is the host's, not d1's: d1 learned only the user's
//!   wakes, and its own send on a guest's line stays held (`d1_gate`'s `not_user`). A guest's line Heimdall itself
//!   was playing through speakers at that moment is its echo, never a wake.
//! - The agent sees every line of a listened guest marked `(listened to)`; other guests stay plain context.
//! - `listen_to` changes a flag only in a run the user's own words woke (his microphone lines, by source, or a typed
//!   message), never one a guest woke or steered, and turns it on only when those words ask to listen to that guest
//!   (`SpeechPolicy::listen_change`), so a guest cannot grant themselves listening. `name_voice` joining a voice to a
//!   listened guest needs the same run.
//! - While any guest is listened to, only a listened guest's or an unlabelled call voice stops Heimdall's speech
//!   (`judge_call_voice` below feeds bridge/call_interrupt.rs); other guests and noise only delay a reply.
use super::*;
use std::collections::BTreeSet;
use std::path::Path;

/// What a listened guest's speaker label carries for the agent.
pub(crate) const MARK: &str = " (listened to)";

/// The listened guests saved in `data`'s `speakers.json` (none when it is missing or unreadable).
pub(super) fn listened_in(data: &Path) -> BTreeSet<String> {
    let path = data.join("speakers.json");
    path.exists().then(|| EnrollmentStore::load(&path).ok()).flatten().map(|s| s.listened()).unwrap_or_default()
}

/// The (speaker, words) lines of an observation: a mixed call final carries one "Name: words" line per run.
fn lines(observation: &agent::protocol::Observation) -> Vec<(&str, &str)> {
    if observation.speaker == "Call" {
        observation.text.split('\n').map(|l| l.split_once(": ").unwrap_or(("Call", l))).collect()
    } else {
        vec![(observation.speaker.as_str(), observation.text.as_str())]
    }
}

impl Pipeline {
    /// Turns listening on or off for the named voice `speaker` (the Speakers switch, or `listen_to` once its
    /// authority is checked). `by`: "menu" or "agent", for the evidence. Returns one line for the agent.
    pub(crate) fn set_listening(&mut self, speaker: &str, on: bool, by: &str) -> Result<String> {
        let wanted = speaker.trim();
        let wanted = wanted.strip_suffix(MARK.trim_start()).map_or(wanted, str::trim);
        ensure!(!wanted.eq_ignore_ascii_case(agent::identity::DEFAULT_USER) && !wanted.eq_ignore_ascii_case(&agent::identity::current().user), "{} is always listened to; that never changes", agent::identity::user_start());
        ensure!(!wanted.eq_ignore_ascii_case("Heimdall"), "Heimdall's own voice is never a guest");
        let shared = self.enrollment_lock.clone();
        let guard = shared.lock().unwrap();
        let mut store = self.load_store()?;
        let Some(name) = store.names.keys().find(|n| n.as_str() == wanted).or_else(|| store.names.keys().find(|n| n.eq_ignore_ascii_case(wanted))).cloned() else {
            if store.voices.contains_key(wanted) {
                anyhow::bail!("{wanted} has no name yet: name them with name_voice first");
            }
            anyhow::bail!("no named voice is called {wanted}");
        };
        let was = store.names[&name].listening;
        if was != on {
            store.set_listening(&name, on)?;
            store.save(&self.data.join("speakers.json"))?;
        }
        drop(guard);
        if was != on {
            self.index.evidence.event("listening_changed", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"speaker": name, "on": on, "by": by}))?;
            for d in self.discord.values() {
                d.actor.send(crate::speakers_runtime::Work::Store(store.clone()))?;
            }
        }
        self.send_speakers();
        Ok(match (on, was == on) {
            (true, false) => format!("Now acting on {name}'s requests: their lines show as \"{name}{MARK}\", and when they address you by name they wake you. It stays on until {} turns it off.", agent::identity::user()),
            (false, false) => format!("No longer acting on {name}'s requests: their lines are context again."),
            (true, true) => format!("Already acting on {name}'s requests."),
            (false, true) => format!("{name}'s requests were not being acted on."),
        })
    }
    /// The agent's `name_voice` from `voice` to `name`: joining a voice to a listened guest, or renaming a listened
    /// guest (its flag moves with it), only in a run the user's own words woke (listen_to would be granted otherwise).
    pub(crate) fn agent_may_name(&self, voice: &str, name: &str) -> Result<()> {
        let listened = self.listened.lock().unwrap().iter().any(|n| n.eq_ignore_ascii_case(name.trim()) || n.eq_ignore_ascii_case(voice.trim()));
        if listened {
            self.tools.speech_policy.listen_change(None, capture::host_now_ns()).map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(())
    }
    /// The user's microphone line `text` (the conversation's turn `seq`) was admitted: if a wake carries it, his words
    /// count for `listen_to` (`owner_wake`).
    pub(super) fn owner_heard(&mut self, seq: u64, text: &str) {
        self.owner_words.push_back((seq, capture::host_now_ns(), text.to_owned()));
        while self.owner_words.len() > 32 {
            self.owner_words.pop_front();
        }
    }
    /// A wake of the user's through `through` went out: his microphone lines it carries (or `fallback`) authorise this run.
    /// `source`: the line that woke. One heard on the phone's live microphone (phone_mic.rs) is a phone turn: its
    /// reply and speech go to the phone under an id Heimdall mints (`remote::phone_mic_wake`).
    pub(super) fn owner_wake(&mut self, source: &str, through: u64, fallback: &str, observation: &str) {
        if self.phone_source(source) {
            crate::remote::phone_mic_wake(Some(observation));
        } else {
            // A wake at the Mac: a phone turn still open is over (remote.rs `mac_turn_started`).
            crate::remote::mac_turn_started_via(crate::remote::MacVia::OwnerVoice);
        }
        let now = capture::host_now_ns();
        let mut his = vec![];
        while self.owner_words.front().is_some_and(|(seq, _, _)| *seq <= through) {
            let (_, at, text) = self.owner_words.pop_front().unwrap();
            // Lines that waited longer than the authority lasts never count (`LISTEN_WORDS_NS`).
            if now.saturating_sub(at) <= crate::bridge::speech_policy::LISTEN_WORDS_NS { his.push(text); }
        }
        self.tools.speech_policy.owner_woke(&if his.is_empty() { fallback.to_owned() } else { his.join(" ") }, capture::host_now_ns());
    }
    /// The listened guest whose line in this call final addresses Heimdall by name, if any. Never the user's microphone,
    /// Heimdall's own audio, or a line whose words Heimdall was playing through speakers then (its echo).
    pub(super) fn guest_address(&mut self, source: &str, observation: &agent::protocol::Observation, words: &[listen::Word]) -> Option<String> {
        if self.user_source(source) || self.own_audio(source) || !self.is_call(source) {
            return None;
        }
        let listened = self.listened.lock().unwrap().clone();
        let guest = lines(observation).into_iter()
            .find(|(name, text)| listened.contains(*name) && !speakers::identity::NOT_GUESTS.iter().any(|g| g.eq_ignore_ascii_case(name)) && decide::send::addressed_by_name(text))
            .map(|(name, _)| name.to_owned())?;
        let (first, last) = (words.first()?, words.last()?);
        let played = self.tools.speech_policy.played_around(first.start_ns, last.end_ns);
        if let Some(v) = (!played.is_empty()).then(|| echo_guard::judge(words, &played)).flatten().filter(|v| v.any()) {
            let _ = self.index.evidence.event("guest_wake_held", source, std::slice::from_ref(&observation.id), last.end_ns,
                serde_json::json!({"speaker": guest, "reason": "heimdalls_echo", "reference": v.reference}));
            return None;
        }
        Some(guest)
    }
    /// The listened guest who said this whole call final, if any (a mixed final's runs never count; only
    /// `guest_address` reads them). Never the user's microphone, Heimdall's own audio, or Heimdall's echo.
    pub(super) fn listened_line(&self, source: &str, observation: &agent::protocol::Observation, words: &[listen::Word]) -> Option<String> {
        if self.user_source(source) || self.own_audio(source) || !self.is_call(source) {
            return None;
        }
        let name = observation.speaker.as_str();
        if !self.listened.lock().unwrap().contains(name) || speakers::identity::NOT_GUESTS.iter().any(|g| g.eq_ignore_ascii_case(name)) {
            return None;
        }
        let (first, last) = (words.first()?, words.last()?);
        let played = self.tools.speech_policy.played_around(first.start_ns, last.end_ns);
        if (!played.is_empty()).then(|| echo_guard::judge(words, &played)).flatten().is_some_and(|v| v.any()) {
            return None;
        }
        Some(name.to_owned())
    }
    /// The speaker tracker's verdict on the voice open on call source `source`, for listening mode's cut-off
    /// (bridge/call_interrupt.rs): the tracks active in it since it began, by their current labels. Nothing while
    /// no guest is listened to or no voice is open; `NO_TRACKER` for a call source with no tracker.
    pub(super) fn judge_call_voice(&self, source: &str) {
        use crate::bridge::call_interrupt::{judge, CallVoiceWho, NO_TRACKER};
        let policy = &self.tools.speech_policy;
        if !policy.listening_guests() {
            return;
        }
        let Some(at) = policy.voice_open_since(source) else { return };
        let Some(d) = self.discord.get(source) else {
            policy.voice_attributed(source, at, CallVoiceWho::MayInterrupt(NO_TRACKER.into()));
            return;
        };
        // A tracker still loading, or a session that began after the voice: wait (`PENDING_MAX` cuts if this lasts).
        if !d.active || d.loading || d.origin > at {
            policy.voice_attributed(source, at, CallVoiceWho::Pending);
            return;
        }
        let rel = |ns: u64| ns.saturating_sub(d.origin) as f64 / 1e9;
        let listened = self.listened.lock().unwrap().clone();
        let who = judge(rel(at), rel(d.heard_until.max(d.origin)), &d.segments, |speaker| {
            let track = TrackId { source: source.to_owned(), session: d.session, speaker };
            // A coded voice first heard in this call, or one near a listened guest, may be that guest on another
            // microphone (listened guests' sets are never widened, identity.rs): treated as unlabelled.
            if let Some((new, near)) = self.coded_tracks.get(&track) {
                if *new || near.as_ref().is_some_and(|n| listened.iter().any(|l| l.eq_ignore_ascii_case(n))) {
                    return None;
                }
            }
            self.speaker_names.get(&track).cloned()
        }, &listened);
        policy.voice_attributed(source, at, who);
    }
    /// The agent's copy of a call final: listened guests' speaker labels carry `MARK`.
    pub(super) fn mark_listened(&self, observation: &mut agent::protocol::Observation) {
        let listened = self.listened.lock().unwrap();
        if listened.is_empty() {
            return;
        }
        if observation.speaker == "Call" {
            observation.text = observation.text.split('\n')
                .map(|line| match line.split_once(": ") {
                    Some((name, text)) if listened.contains(name) => format!("{name}{MARK}: {text}"),
                    _ => line.to_owned(),
                })
                .collect::<Vec<_>>().join("\n");
        } else if listened.contains(&observation.speaker) {
            observation.speaker.push_str(MARK);
        }
    }
}
