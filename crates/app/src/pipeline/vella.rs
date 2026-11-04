//! Dictating to Vella (docs/vella.md, Channel 1): Vella's notifications into `crate::vella::Link`, the island's Vella
//! token, and the dictated utterance's path. While the user dictates, his microphone's words are archived as usual and
//! reach the agent's context marked `[dictated to Vella]`, but never wake it: no early send, no spoken command or
//! heard cue, no d1 decision, no name / "send it" / follow-up wake, no barge-in or talk-over stop.
use super::*;

/// What the agent's copy of a dictated line starts with (the archive keeps his words as said).
pub(crate) const DICTATED_PREFIX: &str = "[dictated to Vella] ";

/// His words split by dictation (`Pipeline::vella_split`).
pub(super) struct VellaSplit {
    /// The words outside dictation: only these may trigger a wake or a command.
    pub kept: Vec<Word>,
    /// The line for the agent and d1, each dictated run marked `[dictated to Vella: …]`.
    pub marked: String,
}

impl Pipeline {
    /// Heimdall's host clock as the link reads it (tests set their own).
    pub(super) fn vella_now(&self) -> u64 {
        #[cfg(test)]
        if let Some(now) = self.hooks.vella_now {
            return now;
        }
        capture::host_now_ns()
    }

    /// One notification from Vella (`crate::vella::observe`). Malformed or another version: journalled and ignored.
    pub fn vella_notice(&mut self, heard: crate::vella::Heard) {
        let Some(notice) = crate::vella::Notice::parse(&heard.info) else {
            let _ = self.index.evidence.event("vella_dictation", "vella", &[], heard.at_ns,
                serde_json::json!({"schema":1,"ignored":"malformed or not version 1","info":heard.info}));
            return;
        };
        let was = self.vella.on();
        let change = self.vella.apply(&notice, heard.at_ns, heard.at_unix);
        // A heartbeat that changes nothing is not journalled (one every 15 s while he dictates).
        if change != Default::default() || notice.reason != crate::vella::Reason::Heartbeat {
            let _ = self.index.evidence.event("vella_dictation", "vella", &[], heard.at_ns,
                serde_json::json!({"schema":1,"notice":notice,"stamp_ns":notice.stamp(heard.at_ns, heard.at_unix),"link":self.vella.summary()}));
        }
        if change.shortcut {
            crate::diagnostics::line("vella", &format!("Vella's dictation shortcut: {}", self.vella.shortcut_label));
        }
        self.vella_shown(was, change.shortcut);
    }

    /// The clock's side: a dictation not heard from in 45 s, or whose Vella is gone, ends (the live loop's settings
    /// tick, and before every check).
    pub fn vella_tick(&mut self) {
        let was = self.vella.on();
        let now = self.vella_now();
        if self.vella.expire(now) {
            let _ = self.index.evidence.event("vella_dictation", "vella", &[], now,
                serde_json::json!({"schema":1,"expired":self.vella.ended_by,"link":self.vella.summary()}));
        }
        self.vella_shown(was, false);
    }

    /// Tells the island when dictation starts or stops (and a keybind change while it shows).
    fn vella_shown(&mut self, was: bool, shortcut: bool) {
        let on = self.vella.on();
        if on != was || (on && shortcut) {
            if on != was {
                crate::diagnostics::line("vella", if on { "dictating to Vella: Heimdall's wakes are held" } else { "Vella dictation ended" });
            }
            self.ui(ui::UiUpdate::Vella { on, keys: self.vella.keys() });
        }
    }

    /// Whether the user is dictating to Vella now, on this source (his microphone).
    pub(super) fn vella_dictating(&mut self, source: &str) -> bool {
        if !self.owner(source) {
            return false;
        }
        self.vella_tick();
        self.vella.on()
    }

    /// Whether these words (or, without words, `start..end`) on his microphone were dictated to Vella: dictation covers
    /// at least half of them.
    pub(super) fn vella_dictated(&mut self, source: &str, words: &[Word], start: u64, end: u64) -> bool {
        if !self.owner(source) {
            return false;
        }
        self.vella_tick();
        let (from, to) = match (words.first(), words.last()) {
            (Some(first), Some(last)) => (first.start_ns, last.end_ns),
            _ => (start, end),
        };
        let now = self.vella_now().max(to);
        self.vella.dictated(from, to, now)
    }

    /// Whether a span on his microphone (a command phrase, `start..end`) was dictated: dictation covers half of it, or
    /// the instant itself when `end <= start`.
    pub(super) fn vella_span_dictated(&mut self, source: &str, start: u64, end: u64) -> bool {
        if !self.owner(source) {
            return false;
        }
        self.vella_tick();
        let now = self.vella_now().max(end);
        self.vella.dictated(start, end, now)
    }

    /// His words split by dictation, word by word (a word is dictated when its middle lies inside a dictation): the
    /// words outside it, which alone may trigger anything, and the line with the dictated runs marked for the agent.
    /// `None` when no word is dictated: everything is as it was.
    pub(super) fn vella_split(&mut self, source: &str, words: &[Word]) -> Option<VellaSplit> {
        if !self.owner(source) || words.is_empty() {
            return None;
        }
        self.vella_tick();
        let now = self.vella_now().max(words.last().map_or(0, |w| w.end_ns));
        let flags: Vec<bool> = words.iter().map(|w| self.vella.covers(w.start_ns / 2 + w.end_ns / 2, now)).collect();
        if !flags.contains(&true) {
            return None;
        }
        let mut parts = vec![];
        let mut i = 0;
        while i < words.len() {
            let d = flags[i];
            let j = (i..words.len()).find(|&k| flags[k] != d).unwrap_or(words.len());
            let text = listen::Transcript::from_words(words[i..j].to_vec()).text;
            parts.push(if d { format!("[dictated to Vella: {text}]") } else { text });
            i = j;
        }
        let kept = words.iter().zip(&flags).filter(|(_, d)| !**d).map(|(w, _)| w.clone()).collect();
        Some(VellaSplit { kept, marked: parts.join(" ") })
    }

    /// The words that may trigger anything: those outside dictation (all of them when none is dictated).
    pub(super) fn vella_undictated(&mut self, source: &str, words: &[Word]) -> Vec<Word> {
        self.vella_split(source, words).map_or_else(|| words.to_vec(), |s| s.kept)
    }

    /// Whether the words outside dictation name Heimdall, say "send it" or fall in the follow-up window: words said
    /// after he stopped (or before he started) wake as always, even in a line mostly dictated.
    pub(super) fn vella_undictated_wakes(&mut self, source: &str, words: &[Word], end_ns: u64) -> bool {
        let kept = self.vella_undictated(source, words);
        if kept.is_empty() {
            return false;
        }
        let text = listen::Transcript::from_words(kept).text;
        let finished = self.tools.send_context.lock().unwrap().finished_ns;
        decide::send::explicit_trigger(&text, true, end_ns, finished).is_some()
    }

    /// Spoken commands proposed from words that turned out dictated: their heard cues leave and their screens are
    /// released; nothing is decided (commands.rs). Commands from his words outside dictation stay.
    pub(super) fn drop_dictated_commands(&mut self, source: &str) {
        let candidates = std::mem::take(&mut self.command_candidates);
        let mut dropped = VecDeque::new();
        for c in candidates {
            if c.source == source && self.vella_span_dictated(source, c.trigger_ns, c.trigger_ns) {
                dropped.push_back(c);
            } else {
                self.command_candidates.push_back(c);
            }
        }
        for c in dropped {
            self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: cue_id(&c.proposals, &c.candidate) }));
            let _ = self.index.evidence.event("command_decision", source, &[], c.trigger_ns,
                serde_json::json!({"schema":1,"kind":"command_decision","phase":"abandoned","reason":"dictated_to_vella","candidate":c.candidate,"proposals":c.proposals,"approved":null}));
            let triggers: Vec<u64> = c.proposals.iter().map(|p| p.candidate.trigger_ns).collect();
            self.screen_holds.take(source, &triggers, c.trigger_ns);
        }
    }

    /// A dictated final: archived as usual with its own status, given to the agent as context (`Command::Observe` and
    /// the conversation's pending text, marked), never decided on and never a wake.
    pub(super) fn give_dictated(&mut self, final_: AgentFinal) -> Result<()> {
        if self.archive_heard_twice(&final_)? {
            return Ok(());
        }
        let AgentFinal { audio_error, observation, source, start, end: end_ns, words, notes, .. } = final_;
        let mut for_agent = observation.clone();
        for_agent.source = self.sources.get(&source).map_or_else(|| display_name_from_id(&source), agent_source_name);
        // Words outside dictation (most of the line was dictated) stay unmarked beside the dictated runs.
        for_agent.text = match self.vella_split(&source, &words) {
            Some(split) if !split.kept.is_empty() => split.marked,
            _ => format!("{DICTATED_PREFIX}{}", observation.text),
        };
        if self.agent.is_none() {
            self.agent = Some(AgentRuntime::start(self.settings.clone(), self.data.clone(), self.session.clone(), self.tools.clone(), self.ui)?);
        }
        self.agent.as_ref().unwrap().send(Command::Observe(for_agent.clone()))?;
        self.index.evidence.event("agent_context_routing", &source, std::slice::from_ref(&observation.id), end_ns,
            serde_json::json!({"schema":1,"given_to_agent":true,"policy":"dictated_to_vella","observation":for_agent}))?;
        {
            let mut context = self.tools.send_context.lock().unwrap();
            let mut observed = serde_json::to_value(&for_agent)?;
            observed["start_ns"] = serde_json::json!(start);
            context.observe_at(&for_agent.speaker, &for_agent.text, end_ns, observed);
            self.tools.persist_send_context(&context)?;
        }
        self.pending_text = self.tools.send_context.lock().unwrap().pending_text().lines().map(str::to_owned).collect();
        let mut record = serde_json::json!({"status":"dictated_to_vella","given_to_agent":true,"would_wake":false});
        if let Some(e) = &audio_error { record["audio_error"] = serde_json::json!(e); }
        self.decisions += 1; // A completed disposition, without inference.
        self.index.record(&observation, &source, start, end_ns, &words, &notes, &record)?;
        crate::diagnostics::event_stream(&serde_json::json!({"event":"final","dry_run":self.settings.dry_run,"id":observation.id,"source":observation.source,"speaker":observation.speaker,"text":observation.text,"decision":record,"would_wake":false}));
        Ok(())
    }
}
