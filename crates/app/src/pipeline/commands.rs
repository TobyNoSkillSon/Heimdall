//! Spoken commands: proposals matched in Parakeet's text (partials, window decodes and finals), the decider's
//! verdict and execution, and the island cues for each.
use super::*;

pub(super) fn overlaps(proposals: &[Proposal], c: &decide::CommandCandidate, origin: &str) -> bool {
    proposals.iter().any(|p| p.candidate.source == c.source
        && p.candidate.trigger_ns.abs_diff(c.trigger_ns) <= COMMAND_MERGE_NS
        && (p.origin != origin || (p.candidate.phrase_guess == c.phrase_guess
            && (p.candidate.trigger_ns == c.trigger_ns || origin == "parakeet_partial"))))
}
/// The transcript note for a screenshot command while Seeing is off.
pub const SCREEN_OFF_NOTE: &str = "[screenshot not taken: screen capture unavailable]";

// The notch island's cues (ui::island): heard at first detection, then done, failed or dropped at the decision.

/// A command's island id: its first detection's trigger time (primary microphone only, so unique per command).
/// The island id of the Listening glyph (command cues use hashes of their detection).
pub(super) const LISTEN_CUE: u64 = u64::MAX;
pub(super) fn cue_id(proposals: &[Proposal], candidate: &decide::CommandCandidate) -> u64 {
    proposals.first().map_or(candidate.trigger_ns, |p| p.candidate.trigger_ns)
}
/// The icon a detected phrase announces before the decision.
pub(super) fn cue_kind(phrase: &str) -> ui::CommandKind {
    let p = phrase.to_lowercase();
    if p.contains("screenshot") || p.contains("capture") {
        ui::CommandKind::Screenshot
    } else {
        ui::CommandKind::LookAtThis
    }
}
/// The outcome of an approved command, as the island shows it.
pub(super) fn action_cue(id: u64, command: decide::CommandSelection, result: Result<(), &anyhow::Error>, screen_off: bool, _dry_run: bool) -> ui::CommandCue {
    use decide::CommandSelection as C;
    use ui::CommandKind as K;
    let kind = match command {
        C::TakeScreenshot => K::Screenshot,
        C::LookAtThis => K::LookAtThis,
        // Escapes never execute: nothing to confirm.
        C::OtherCommand | C::Unsure => return ui::CommandCue::Dropped { id },
    };
    let failed = |reason: &str| ui::CommandCue::Failed { id: Some(id), kind, reason: reason.into() };
    match result {
        Ok(()) if screen_off && matches!(kind, K::Screenshot | K::LookAtThis) => failed("Seeing is off"),
        Ok(()) => ui::CommandCue::Done { id: Some(id), kind, label: kind.done_label().into() },
        Err(e) => {
            let e = format!("{e:#}");
            failed(if e.contains("disk floor") {
                "Disk almost full"
            } else {
                "Couldn't run"
            })
        }
    }
}

impl Pipeline {
    /// Ok(true) when the candidate is new or merged into one still waiting for its decision.
    pub(super) fn propose_from(&mut self, candidate: decide::CommandCandidate, origin: &'static str) -> Result<bool> {
        let ids = self.active.get(&candidate.source).map(|u| vec![format!("{}:{}", self.session.file_name().unwrap().to_string_lossy(), u.id)]).unwrap_or_default();
        let eligible = self.primary_mic(&candidate.source);
        self.index.evidence.event("command_decision", &candidate.source, &ids, candidate.trigger_ns,
            serde_json::json!({"schema":1,"kind":"command_decision","phase":"candidate","candidate":candidate,"origin":origin,
                "eligible":eligible,"error":if eligible {None}else{Some("not_primary_microphone")}}))?;
        ensure!(eligible, "command candidates require the primary microphone");
        let proposal = Proposal { origin, candidate: candidate.clone() };
        // A later detection of a command already decided (the window or final after a partial) is not a new one.
        if let Some(done) = self.completed_commands.iter_mut().rev().find(|d| (ids.is_empty() || ids.contains(&d.utterance_id)) && (overlaps(&d.proposals, &candidate, origin)
            || (!ids.is_empty() && d.approved == Some(decide::CommandSelection::for_phrase(&candidate.phrase_guess))))) {
            done.proposals.push(proposal);
            self.index.evidence.event("command_decision", &candidate.source, std::slice::from_ref(&done.utterance_id), candidate.trigger_ns,
                serde_json::json!({"schema":1,"kind":"command_decision","phase":"merged","proposals":done.proposals,"reason":"already_decided","approved":null}))?;
            return Ok(false);
        }
        let guess = candidate.phrase_guess.clone();
        if let Some(pending) = self.command_candidates.iter_mut().find(|p| overlaps(&p.proposals, &candidate, origin)
            && p.proposals.iter().filter(|p| p.origin == origin).all(|p|
                p.candidate.trigger_ns == candidate.trigger_ns || (origin == "parakeet_partial"
                    && p.candidate.phrase_guess == candidate.phrase_guess))) {
            pending.proposals.push(proposal);
            // Prefer verified final word timing over an earlier detection's trigger.
            if origin == "parakeet_final" { pending.candidate = candidate; }
        } else {
            ensure!(self.command_candidates.len() < 256, "command candidate queue full");
            let id = candidate.trigger_ns;
            self.command_candidates.push_back(PendingCommand { candidate, proposals: vec![proposal] });
            self.ui(ui::UiUpdate::Command(ui::CommandCue::Heard { id, kind: cue_kind(&guess) }));
        }
        Ok(true)
    }
    /// A side transcript arrived (speech_split asks for its own; anything else is stale).
    pub(super) fn side_transcript(&mut self, _id: u64, _result: Result<listen::Transcript, String>, _decode_ms: f64) -> Result<()> {
        // Barge-in no longer reads words (decision 31): a side transcript nobody else asked for is stale.
        Ok(())
    }
    /// Pin until the decision: a screenshot's phrase-time ring frames plus a full-resolution frame
    /// per display (grabbed off this thread).
    pub(super) fn hold_screen(&mut self, source: &str, anchor_ns: u64, phrase_start_ns: Option<u64>, event_ns: u64) {
        let now = if self.replay { event_ns } else { capture::host_now_ns() };
        self.screen_holds.pin(&self.screen_buffer, source, anchor_ns, phrase_start_ns, now);
    }
    /// A window decode's stable words: every fixed command found here is proposed and decided now, from
    /// these words, with its screen held from the phrase.
    pub(super) fn window_commands(&mut self, s: &str, words: &[Word], event_ns: u64) -> Result<()> {
        if !self.primary_mic(s) || self.heard_own_sound(s, words, 0, 0) {
            return Ok(());
        }
        // Words dictated to Vella are never a spoken command: only his words outside dictation are matched, and a
        // proposal from words that turned out dictated leaves (pipeline/vella.rs).
        let undictated = self.vella_undictated(s, words);
        if undictated.len() < words.len() {
            self.drop_dictated_commands(s);
        }
        let words = &undictated[..];
        let kept = self.without_own_sound(s, words);
        let words = &kept[..];
        let id = self.active[s].id.to_string();
        let timed: Vec<_> = words.iter().map(screen::TimedWord::from).collect();
        for m in self.command_window.observe_utterance(s, &id, &timed, false) {
            let candidate = decide::CommandCandidate { source: s.into(), trigger_ns: m.trigger_ns, phrase_guess: m.phrase.clone(), score: 1.0 };
            // Both commands are screen commands: hold the screen from the phrase.
            if self.propose_from(candidate, "parakeet_window")? {
                self.hold_screen(s, m.trigger_ns, Some(m.start_ns), event_ns);
            }
        }
        let mut pending = std::mem::take(&mut self.command_candidates);
        while let Some(c) = pending.pop_front() {
            if c.source != s || !c.proposals.iter().any(|p| p.origin == "parakeet_window") {
                self.command_candidates.push_back(c);
                continue;
            }
            if let Err(e) = self.decide_command(c, words, decide::CommandStage::Early, serde_json::json!({"origin":"window_decode"})) {
                self.command_candidates.extend(pending);
                return Err(e);
            }
        }
        Ok(())
    }

}
impl Pipeline {
    pub(super) fn reset_commands(&mut self, source: &str) {
        self.reset_barge(source);
        self.completed_commands.retain(|c| c.source != source);
        self.command_screen.reset_source(source);
        self.screen_holds.reset_source(source);
        self.command_window.reset_source(source);
        // Release tentative feedback as well as the held pixels on capture loss.
        for candidate in self.command_candidates.iter().filter(|c| c.source == source) {
            self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id:cue_id(&candidate.proposals,&candidate.candidate) }));
            if let Err(e) = self.index.evidence.event("command_decision",source,&[],candidate.trigger_ns,
                serde_json::json!({"schema":1,"kind":"command_decision","phase":"abandoned","candidate":candidate.candidate,"proposals":candidate.proposals,"error":"source closed or capture gap before the decision","approved":null})) { eprintln!("Archive abandoned command: {e}"); }
        }
        self.command_candidates.retain(|c| c.source != source);
    }
    /// The turn's final: phrases first found here are proposed, and every command of this turn still
    /// undecided is decided from the final's words.
    pub(super) fn commands(&mut self, s: &str, words: &[Word], final_: bool) -> Result<()> {
        if !self.primary_mic(s) { return Ok(()); }
        // Heimdall's own sound on the microphone ("Let me take a look.") is never a command (pipeline/ack.rs).
        if self.heard_own_sound(s, words, 0, 0) { return Ok(()); }
        // Words dictated to Vella are never a spoken command: only his words outside dictation are matched, and a
        // proposal from words that turned out dictated leaves (pipeline/vella.rs).
        let undictated = self.vella_undictated(s, words);
        if undictated.len() < words.len() {
            self.drop_dictated_commands(s);
        }
        let words = &undictated[..];
        let kept = self.without_own_sound(s, words);
        let words = &kept[..];
        if !final_ {
            let timed: Vec<_> = words.iter().map(screen::TimedWord::from).collect();
            for m in screen::PhraseMatcher::new(COMMAND_PHRASES).observe_utterance(s, "partial", &timed, true) {
                let candidate = decide::CommandCandidate { source:s.into(), trigger_ns:m.trigger_ns, phrase_guess:m.phrase.clone(), score:1. };
                if self.propose_from(candidate, "parakeet_partial")? {
                    self.hold_screen(s,m.trigger_ns,Some(m.start_ns),m.trigger_ns);
                }
            }
            return Ok(());
        }
        let id = self.active[s].id.to_string();
        let timed: Vec<_> = words.iter().map(screen::TimedWord::from).collect();
        let matches = self.command_screen.observe_utterance(s, &id, &timed, true);
        // PhraseMatcher coalesces nested aliases; preserve each distinct occurrence.
        for m in &matches {
            self.propose_from(
                decide::CommandCandidate {
                    source: s.into(),
                    trigger_ns: m.trigger_ns,
                    phrase_guess: m.phrase.clone(),
                    score: 1.0,
                },
                "parakeet_final",
            )?;
        }
        let mut pending = std::mem::take(&mut self.command_candidates);
        while let Some(c) = pending.pop_front() {
            if c.source != s || words.last().is_none_or(|w| c.trigger_ns > w.end_ns.saturating_add(COMMAND_MERGE_NS)) {
                self.command_candidates.push_back(c);
                continue;
            }
            if c.proposals.iter().all(|p| p.origin == "parakeet_partial") {
                self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id:cue_id(&c.proposals,&c.candidate) }));
                self.index.evidence.event("command_decision",s,&[],c.trigger_ns,
                    serde_json::json!({"phase":"abandoned","reason":"partial_phrase_retracted","candidate":c.candidate,"approved":null}))?;
                continue;
            }
            if let Err(e) = self.decide_command(c, words, decide::CommandStage::Final, serde_json::json!({"origin":"final"})) {
                // The rest were taken from the queue; keep them for a later final.
                self.command_candidates.extend(pending);
                return Err(e);
            }
        }
        if let Some(w) = words.last() {
            self.screen_holds.release_through(s, w.end_ns.saturating_add(COMMAND_MERGE_NS));
        }
        Ok(())
    }
    /// Decide one command from text: Parakeet's words for the last <=15 s of this turn up to 200 ms after
    /// the trigger. Yes: the island confirms and the command runs on the screen held at the phrase.
    /// Anything else: nothing shows and its holds are released.
    pub(super) fn decide_command(
        &mut self,
        c: PendingCommand,
        words: &[Word],
        stage: decide::CommandStage,
        side: serde_json::Value,
    ) -> Result<()> {
        let s = c.source.clone();
        let candidate = c.candidate.clone();
        let proposals = c.proposals;
        let queued_cue = cue_id(&proposals, &candidate);
        let utterance_id = format!(
            "{}:{}",
            self.session.file_name().unwrap().to_string_lossy(),
            self.active[&s].id
        );
        let selection = decide::CommandSelection::for_phrase(&candidate.phrase_guess);
        // Repeated requests for the same screen/cancel action in one spoken turn are one action, even >1 s apart.
        if self.completed_commands.iter().any(|d| {
                d.utterance_id == utterance_id && d.source == s && d.approved == Some(selection)
            })
        {
            self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped {
                id: cue_id(&proposals, &candidate),
            }));
            let triggers: Vec<_> = proposals.iter().map(|p| p.candidate.trigger_ns).collect();
            self.screen_holds.take(&s, &triggers, candidate.trigger_ns);
            self.index.evidence.event("command_decision",&s,std::slice::from_ref(&utterance_id),candidate.trigger_ns,
                serde_json::json!({"phase":"merged","reason":"same_command_in_utterance","candidate":candidate,"proposals":proposals,"approved":null}))?;
            return Ok(());
        }
        self.completed_commands.push_back(CompletedCommand {
            source: s.clone(),
            utterance_id: utterance_id.clone(),
            proposals: proposals.clone(),
            approved: None,
        });
        while self.completed_commands.len() > 256 {
            self.completed_commands.pop_front();
        }
        let now = if self.replay {
            words
                .last()
                .map_or(candidate.trigger_ns, |w| w.end_ns.max(candidate.trigger_ns))
        } else {
            capture::host_now_ns()
        };
        let end = candidate.trigger_ns.saturating_add(SIDE_AFTER_NS);
        let from = end.saturating_sub(SIDE_MAX_NS).max(self.active[&s].start);
        let window: Vec<Word> = words
            .iter()
            .filter(|w| w.end_ns > from && w.start_ns <= end)
            .cloned()
            .collect();
        let text = listen::Transcript::from_words(window.clone()).text;
        // Where the phrase begins, for the screen frames: the matched phrase nearest the trigger.
        let timed: Vec<_> = window.iter().map(screen::TimedWord::from).collect();
        let phrase_start = screen::PhraseMatcher::new(COMMAND_PHRASES)
            .observe_utterance(&s, "decision", &timed, true)
            .into_iter()
            .min_by_key(|m| m.trigger_ns.abs_diff(candidate.trigger_ns))
        .map_or(
            candidate.trigger_ns.saturating_sub(PHRASE_FALLBACK_NS),
            |m| m.start_ns.min(candidate.trigger_ns),
        );
        let input = decide::DecisionInput {
            source: decide::DecisionSource::Microphone,
            is_final: stage == decide::CommandStage::Final,
            current: decide::Turn {
                id: utterance_id.clone(),
                role: decide::Role::Owner,
                transcript: text.clone(),
                // Transcript only; the archived waveform stays on the capture path.
                audio: decide::AudioClip::default(),
            },
            context: self
                .context
                .iter()
                .filter(|t| t.id != utterance_id)
                .map(text_turn)
                .collect(),
            // Newest unsent speech within the decider's bound; a restored backlog can be larger (utterances.rs).
            pending_transcript: decide::newest_suffix(&self.pending_text.iter().map(String::as_str).collect::<Vec<_>>().join("\n"), decide::MAX_PENDING_BYTES)
                .to_owned(),
            agent_state: if self.settings.dry_run {
                "dry-run"
            } else {
                "collecting"
            }
            .into(),
            media: decide::DecisionMedia::Text,
            questions: vec![decide::Question::CommandCheck(
                decide::CommandSelection::for_phrase(&candidate.phrase_guess),
            )],
        };
        #[cfg(test)]
        let injected = self.command_test_answers.pop_front();
        #[cfg(not(test))]
        let injected: Option<(decide::CommandIntent, decide::CommandSelection)> = None;
        let infer_candidate = candidate.clone();
        let triggers: Vec<_> = proposals
            .iter()
            .map(|p| p.candidate.trigger_ns)
            .chain([candidate.trigger_ns])
            .collect();
        let hold = self.screen_holds.take(&s, &triggers, phrase_start);
        let expected = self.active[&s].id;
        let source = s.clone();
        let complete = move |this: &mut Pipeline,
                             record: decide::CommandDecision,
                             runtime: Option<String>|
              -> Result<()> {
            let cue = cue_id(&proposals, &candidate);
            if !this.active.get(&s).is_some_and(|u| u.id == expected) {
                // The turn ended (or its source closed) before the verdict: nothing runs, and the heard glyph leaves
                // with it rather than resting at the notch (night review I1: every exit clears its cue).
                this.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: cue }));
                this.index.evidence.event("command_decision", &s, std::slice::from_ref(&utterance_id), candidate.trigger_ns,
                    serde_json::json!({"schema":1,"kind":"command_decision","phase":"abandoned","reason":"turn_ended_before_verdict","candidate":candidate,"approved":null}))?;
                return Ok(());
            }
            // The phrase itself was dictated to Vella (proposed before Vella's start arrived, stamped back to the
            // press): it never runs, even if dictation has stopped since; a phrase said before he started still runs
            // (pipeline/vella.rs).
            if this.vella_span_dictated(&s, phrase_start.min(candidate.trigger_ns), candidate.trigger_ns) {
                this.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: cue }));
                this.index.evidence.event("command_decision", &s, std::slice::from_ref(&utterance_id), candidate.trigger_ns,
                    serde_json::json!({"schema":1,"kind":"command_decision","phase":"abandoned","reason":"dictated_to_vella","candidate":candidate,"approved":null}))?;
                return Ok(());
            }
            // Persist authorization before execution; action records refer to this candidate.
            let persisted = (|| -> Result<()> {
                let mut payload = serde_json::to_value(&record)?;
                payload["kind"] = serde_json::json!("command_decision");
                payload["phase"] = serde_json::json!("chain");
                payload["transcript"] = serde_json::json!(text);
                // The side transcript is the decider's input and lives only here (the user's archive rule keeps model inputs).
                let mut side = side;
                side["words"] = serde_json::json!(window);
                payload["decider_text"] = side;
                payload["runtime"] = serde_json::json!(runtime);
                payload["phrase_start_ns"] = serde_json::json!(phrase_start);
                payload["proposals"] = serde_json::json!(proposals);
                this.index.evidence.event(
                    "command_decision",
                    &s,
                    std::slice::from_ref(&utterance_id),
                    candidate.trigger_ns,
                    payload.clone(),
                )?;
                // Decider training examples are the personal edition's; the public app keeps nothing under bugs/.
                if record.needs_training() && !crate::edition::PUBLIC {
                    this.bugs.write_auto_command(&payload)?;
                }
                Ok(())
            })();
            let triggers: Vec<u64> = proposals
                .iter()
                .map(|p| p.candidate.trigger_ns)
                .chain([candidate.trigger_ns])
                .collect();
            let _ = triggers;
            if let Err(e) = persisted {
                this.ui(ui::UiUpdate::Command(ui::CommandCue::Failed {
                    id: Some(cue),
                    kind: cue_kind(&candidate.phrase_guess),
                    reason: "Couldn't run".into(),
                }));
                return Err(e);
            }
            if let Some(error) = &record.error {
                this.errors += 1;
                this.component_status(
                    "command-decision",
                    Some(format!("Command not acted on: {error}")),
                );
                this.ui(ui::UiUpdate::Command(ui::CommandCue::Failed {
                    id: Some(cue),
                    kind: cue_kind(&candidate.phrase_guess),
                    reason: "Couldn't decide".into(),
                }));
            } else {
                this.component_status("command-decision", None);
            }
            // A tentative heard cue never claims execution; rejection removes it.
            let Some(command) = record.approved else {
                this.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: cue }));
                return Ok(());
            };
            if let Some(done) = this.completed_commands.iter_mut().find(|d| {
                d.utterance_id == utterance_id
                    && d.proposals
                        .iter()
                        .any(|p| p.candidate.trigger_ns == candidate.trigger_ns)
            }) {
                done.approved = Some(command);
            }
            let matched = screen::PhraseMatch {
                source: s.clone(),
                phrase: candidate.phrase_guess.clone(),
                start_ns: phrase_start,
                trigger_ns: candidate.trigger_ns,
            };
            let action = this.execute_command(
                command,
                &matched,
                &utterance_id,
                now,
                hold,
            );
            crate::diagnostics::command_record(
                &this.evidence,
                cue,
                &utterance_id,
                &format!("{command:?}"),
                this.ui.is_some(),
            );
            this.ui(ui::UiUpdate::Command(action_cue(
                cue,
                command,
                action.as_ref().map(drop),
                this.settings.screen_off,
                this.settings.dry_run,
            )));
            this.index.evidence.event("command_action", &s, std::slice::from_ref(&utterance_id), candidate.trigger_ns,
            serde_json::json!({"candidate":candidate,"command":command,"stage":stage,"error":action.as_ref().err().map(|e|format!("{e:#}")),"dry_run":this.settings.dry_run}))?;
            match action {
                Err(e) => {
                    this.errors += 1;
                    this.component_status(
                        "command-action",
                        Some(format!("Command unavailable: {e}")),
                    );
                }
                Ok(()) => this.component_status("command-action", None),
            }
            Ok(())
        };
        if let Some((intent, _)) = injected {
            let record = decide::command_chain_at(infer_candidate.clone(), &input, stage, |_| {
                let answer = decide::Answer::CommandIntent {
                    intent,
                    probabilities: decide::CommandIntent::ALL
                        .into_iter()
                        .map(|c| (c.key().into(), if c == intent { 1. } else { 0. }))
                        .collect(),
                };
                (
                    Ok((vec![answer], 0)),
                    Some(serde_json::json!({"runtime":"test_injection"})),
                )
            });
            complete(self, record, None)
        } else {
            *self.command_inflight.entry(source.clone()).or_default() += 1;
            let s_for_failure = source.clone();
            // An unavailable decision owner is a failure to queue like any other: it must not skip the cue's exit (I1).
            let queued = self.inference().and_then(|owner| owner.request(2, input.token_bound(), move |state| {
                let (record, runtime) = match state.decider() {
                    Ok(d) => (
                        d.command_decision_at(infer_candidate.clone(), &input, stage),
                        Some(d.command_runtime().to_owned()),
                    ),
                    Err(e) => (
                        decide::command_chain_at(infer_candidate.clone(), &input, stage, |_| {
                            (Err(anyhow::anyhow!("{e:#}")), None)
                        }),
                        None,
                    ),
                };
                state.outcome(record.error.is_none());
                (record,runtime)
            }, move |p,(record,runtime)| {
                    let result = complete(p, record, runtime);
                    if let Some(n) = p.command_inflight.get_mut(&source) {
                        *n = n.saturating_sub(1);
                    }
                    let drained = p.drain_command_events(&source);
                    result.and(drained)
            }));
            if queued.is_err() {
                if let Some(n) = self.command_inflight.get_mut(&s_for_failure) {
                    *n = n.saturating_sub(1);
                }
                // Never asked: the heard glyph leaves (I1).
                self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: queued_cue }));
            }
            queued
        }
    }
    pub(super) fn execute_command(
        &mut self,
        command: decide::CommandSelection,
        matched: &screen::PhraseMatch,
        utterance_id: &str,
        now: u64,
        hold: Option<screen::ScreenHold>,
    ) -> Result<()> {
        use decide::CommandSelection as C;
        let s = &matched.source;
        match command {
            // Seeing is off: nothing is captured, so the command says so instead of failing or waiting.
            C::TakeScreenshot | C::LookAtThis if self.settings.screen_off => {
                self.index.evidence.event(
                    "screen_command",
                    s,
                    &[utterance_id.into()],
                    now,
                    serde_json::json!({"utterance_id":utterance_id,"final":true,"screen_off":true}),
                )?;
                self.notes += 1;
                self.active.get_mut(s).unwrap().notes.push(SCREEN_OFF_NOTE.into());
                self.component_status("screen-command", None);
            }
            C::TakeScreenshot | C::LookAtThis => {
                let guard = archive::DiskGuard {
                    live_floor: None,
                    dir: archive::store_root(&self.session),
                    floor: self.settings.free_space_floor_bytes,
                    status: self.disk_status.clone(),
                };
                let action = if !guard.allows(256 * 1024 * 1024) {
                    Err(anyhow::anyhow!("below disk floor"))
                } else {
                    // Held since detection; a phrase first seen in this final is pinned now.
                    let hold = match hold {
                        Some(hold) => hold,
                        None => self.screen_holds.pin_now(&self.screen_buffer, s, matched.trigger_ns, matched.start_ns, now),
                    };
                    screen::attach_held(hold, matched, now, &archive::run_dir(&self.data, &self.session).join("shots"))
                };
                let event = match action {
                    Ok(a) => screen::ScreenActionEvent::Action(a),
                    Err(e) => screen::ScreenActionEvent::Failed {
                        source: s.clone(),
                        trigger_ns: matched.trigger_ns,
                        error: e.to_string(),
                    },
                };
                self.index.evidence.event(
                    "screen_command",
                    s,
                    &[utterance_id.into()],
                    now,
                    serde_json::json!({"utterance_id":utterance_id,"final":true,"action":event}),
                )?;
                self.notes += 1;
                match event {
                    screen::ScreenActionEvent::Action(a) => {
                        // While drawing mode is up, the screenshot carries the user's strokes.
                        crate::drawing::mark_shots(&a);
                        for error in &a.full_resolution_errors {
                            eprintln!("Full-resolution screenshot unavailable: {error}");
                        }
                        self.component_status(
                            "screen-command",
                            if a.full_resolution_errors.is_empty() {
                                None
                            } else {
                                Some("Full-resolution screenshot unavailable".into())
                            },
                        );
                        let (images, _) = crate::bridge::archive_screenshot_action(
                            &self.index.evidence,
                            &a,
                            s,
                            &[utterance_id.into()],
                            now,
                        )?;
                        let u = self.active.get_mut(s).unwrap();
                        u.notes.push(a.note);
                        u.images.extend(images);
                    }
                    screen::ScreenActionEvent::Failed { error, .. } => {
                        self.active
                            .get_mut(s)
                            .unwrap()
                            .notes
                            .push(format!("[screenshot unavailable: {error}]"));
                        anyhow::bail!(error);
                    }
                }
            }
            C::OtherCommand | C::Unsure => anyhow::bail!("escape cannot execute"),
        }
        Ok(())
    }
}
