//! Listen events into utterances: admission before commands, finals handed to the agent, waiting calls.
use super::*;

/// The caption a late screenshot carries into the running turn: its words and the clock time they were said (E9.1).
pub(super) fn late_screenshot_caption(words: &str, said_ms: u64) -> String {
    format!("Screenshot for \"{words}\" ({})", agent::clock::local_hms(said_ms))
}

impl Pipeline {
    pub(super) fn listen(&mut self, e: ListenEvent) -> Result<()> {
        let (source, ns) = match &e {
            ListenEvent::SpeechStart {
                source,
                host_time_ns,
            }
            | ListenEvent::SpeechEnd {
                source,
                host_time_ns,
            }
            | ListenEvent::Discontinuity {
                source,
                host_time_ns,
            } => (source, *host_time_ns),
            ListenEvent::PartialTranscript { source, words, .. } => {
                (source, words.first().map_or(0, |w| w.start_ns))
            }
            ListenEvent::TranscriptSegment {
                source, start_ns, ..
            }
            | ListenEvent::FinalUtterance {
                source, start_ns, ..
            } => (source, *start_ns),
        };
        let event_end = match &e {
            ListenEvent::FinalUtterance { end_ns, .. } | ListenEvent::TranscriptSegment { end_ns, .. } => *end_ns,
            ListenEvent::PartialTranscript { words, .. } => words.last().map_or(ns, |w| w.end_ns),
            _ => ns,
        };
        let obsolete = !self.sources.contains_key(source) || self.source_cutoffs.get(source).is_some_and(|cutoff| {
            if matches!(&e, ListenEvent::FinalUtterance { .. }) { event_end <= *cutoff } else { ns < *cutoff }
        });
        if obsolete {
            self.index.evidence.event("obsolete_worker_event", source, &[], ns,
                serde_json::json!({"event":e,"cutoff_ns":self.source_cutoffs.get(source),"reason":if self.sources.contains_key(source){"before_gap_boundary"}else{"source_closed"}}))?;
            return Ok(());
        }
        // A drop onto the notch waits while his utterance is open (`drops.rs`).
        self.drop_listen(&e);
        {
            self.index.evidence.note(
                "listen_event",
                source,
                &self
                    .active
                    .get(source)
                    .map(|u| {
                        vec![format!(
                            "{}:{}",
                            self.session.file_name().unwrap().to_string_lossy(),
                            u.id
                        )]
                    })
                    .unwrap_or_default(),
                ns,
                serde_json::to_value(&e)?,
            )?;
        }
        let _explicit_hold=if matches!(e,ListenEvent::FinalUtterance {..}) {Some(self.inference()?.hold())}else{None};
        // Latency map (lab/latency): the conductor received this final.
        if let ListenEvent::FinalUtterance { source, end_ns, forced, .. } = &e { if !self.replay { crate::latency::received(source, *end_ns, *forced); } }
        self.admit_before_command(&e)?;
        if self.command_inflight.get(source).copied().unwrap_or(0)>0 && !matches!(e,ListenEvent::Discontinuity {..}) {
            let source=source.to_owned();
            self.command_deferred.entry(source).or_default().push_back(e);
            return Ok(());
        }
        let retry_event=e.clone();
        match e {
            ListenEvent::SpeechStart {
                source,
                host_time_ns,
            } => {
                if self.live_worker.is_none() { if let Some(night) = &self.night { night.activity().speech(&source, true); } }
                self.quiet_since.remove(&source);
                if self.is_call(&source) {
                    // Another participant is talking: the call is live while this voice lasts (bridge/speech_policy.rs).
                    self.tools.speech_policy.other_voice(&source, Some(host_time_ns));
                    self.start_discord(&source, host_time_ns)?;
                    if !self.replay && self.sources.get(&source).is_some_and(is_discord_bundle) && speakers::ax::is_trusted() {
                        let mut snapshots = vec![];
                        for pid in speakers::ax::discord_processes() {
                            if let Ok(ax) =
                                speakers::ax::snapshot(pid, capture::host_now_ns() as f64 / 1e9)
                            {
                                snapshots.push(ax);
                            }
                        }
                        if let Some(d) = self.discord.get(&source) {
                            d.actor.send(crate::speakers_runtime::Work::Ax(snapshots))?;
                        }
                    }
                }
                if self.owner(&source) {
                    self.mic_voice_warm(&source);
                    // Talking over an acknowledgement stops it (pipeline/ack.rs); read before his voice is noted. Not
                    // while he dictates to Vella (pipeline/vella.rs).
                    if !self.vella_dictating(&source) { self.ack_speech_start(&source); }
                    self.island_hold_voice(&source, true, host_time_ns);
                    self.tools.voice(&source, true);
                }
                self.barge_speech_start(&source, host_time_ns)?;
                if !self.active.contains_key(&source) {
                    let id = self.early_ids.remove(&(source.clone(),host_time_ns)).unwrap_or_else(||{let id=self.next_id;self.next_id+=1;id});
                    self.active.insert(
                        source,
                        Utterance {
                            id,
                            start: host_time_ns,
                            ..Default::default()
                        },
                    );
                }
            }
            ListenEvent::SpeechEnd {
                source,
                host_time_ns,
            } => {
                if self.live_worker.is_none() { if let Some(night) = &self.night { night.activity().speech(&source, false); } }
                self.tools.voice(&source, false);
                self.tools.speech_policy.other_voice(&source, None);
                if let Some(d) = self.discord.get_mut(&source) {
                    d.speaking = false;
                }
                self.barge_speech_end(&source, host_time_ns)?;
                // Whose voice this was is asked now, so the final rarely waits for it (mic_voice.rs).
                self.mic_voice_prefetch(&source, host_time_ns);
                self.island_hold_voice(&source, false, host_time_ns);
                self.quiet_since.insert(source, host_time_ns);
            }
            ListenEvent::Discontinuity {
                source,
                host_time_ns,
            } => self.gap(&source, host_time_ns, "listen clock discontinuity"),
            ListenEvent::TranscriptSegment {
                source,
                words,
                start_ns,
                end_ns,
                ..
            } => {
                self.ensure_utterance(&source, start_ns);
                self.segments += 1;
                self.index.note("transcript_segment",&source,start_ns,&serde_json::json!({"utterance_id":self.active[&source].id,"end_ns":end_ns,"words":words}))?;
                let u = self.active.get_mut(&source).unwrap();
                u.prefix.extend(words);
                let discard = u.prefix.len().saturating_sub(4096);
                u.prefix.drain(..discard);
                let all = self.active[&source].prefix.clone();
                if let Some(track) = self.track_for(&source, start_ns, end_ns)? {
                    let text = listen::Transcript::from_words(all.clone()).text;
                    self.discord[&source]
                        .actor
                        .send(crate::speakers_runtime::Work::Observe(
                            track,
                            end_ns as f64 / 1e9,
                            text,
                            vec![],
                        ))?;
                }
                self.commands(&source, &all, false)?;
                self.window_commands(&source, &all, end_ns)?;
                self.line(&source, &all, false, None);
            }
            ListenEvent::PartialTranscript { source, words, .. } => {
                self.ensure_utterance(&source, words.first().map_or(0, |w| w.start_ns));
                let mut all = self.active[&source].prefix.clone();
                all.extend(words);
                self.commands(&source, &all, false)?;
                self.line(&source, &all, false, None);
            }
            ListenEvent::FinalUtterance {
                source,
                words,
                start_ns,
                end_ns,
                forced,
                end_of_turn_probability,
                ..
            } => {
                let _inference_hold=self.inference()?.hold();
                self.ensure_utterance(&source, start_ns);
                let mut all = self.active[&source].prefix.clone();
                all.extend(words);
                // Dictated to Vella (at least half of it): archived and given as context, never a wake. His words outside
                // dictation that name Heimdall, say "send it" or follow up still wake as always (pipeline/vella.rs).
                let dictated = self.vella_dictated(&source, &all, start_ns, end_ns) && !self.vella_undictated_wakes(&source, &all, end_ns);
                self.commands(&source, &all, true)?;
                if self.command_inflight.get(&source).copied().unwrap_or(0)>0 {
                    self.command_deferred.entry(source).or_default().push_back(retry_event);
                    return Ok(());
                }
                let track = self.track_for(&source, start_ns, end_ns)?;
                // speech-policy (night 3): an utterance on the user's microphone lying wholly inside the wake sound's
                // window is the sound itself: Heimdall's, archived only (bridge/speech_policy.rs).
                let wake_sound = self.heard_own_sound(&source, &all, start_ns, end_ns);
                // Echo guard (23 Oct): Heimdall's reply echoed through speakers is its own voice (above); his words over
                // it stay his, without the echo words (pipeline/echo_guard.rs).
                let echo = self.echo_verdict(&source, &all).filter(|v| v.any());
                let utterance_id = format!("{}:{}", self.session.file_name().unwrap().to_string_lossy(), self.active[&source].id);
                if let Some(v) = &echo {
                    self.record_echo(&source, &utterance_id, &all, v);
                }
                // Whose voice it is (mic_voice.rs): his microphone is his only in his voice; Heimdall's own voice is
                // archived only, like its echo; another voice is "Unknown". Over Heimdall's echo, only his kept words
                // are judged.
                // Only his primary microphone: his print and the thresholds were measured there.
                let voice = (self.primary_mic(&source) && !wake_sound).then(|| {
                    let start = self.active[&source].start;
                    let words = all.clone();
                    self.mic_voice(&source, &utterance_id, start, end_ns, &words, echo.as_ref()).voice
                });
                // Only Heimdall's own score makes a line Heimdall's: his words over the echo that fail his print are
                // "Unknown", context that never wakes, never thrown away (review 30 Oct).
                let own_voice = voice == Some(heimdall_voice::MicVoice::Heimdall);
                let unknown = voice == Some(heimdall_voice::MicVoice::Unknown);
                if let Some(v) = &echo {
                    if !v.own() && !own_voice {
                        all = v.kept(&all);
                    }
                }
                let wake_sound = wake_sound || own_voice;
                let speaker = track
                    .as_ref()
                    .map(|t| {
                        self.speaker_names
                            .get(t)
                            .cloned()
                            .unwrap_or_else(|| format!("Speaker {}", t.speaker))
                    })
                    .unwrap_or_else(|| {
                        if wake_sound {
                            "Heimdall".into()
                        } else if unknown {
                            "Unknown".into()
                        } else if self.owner(&source) {
                            "User".into()
                        } else if self.own_audio(&source) {
                            "Heimdall".into()
                        } else if self.is_call(&source) {
                            "Speaker ?".into()
                        } else {
                            "audio".into()
                        }
                    });
                let text = listen::Transcript::from_words(all.clone()).text;
                if let Some(t) = &track {
                    let mut ax_items = vec![];
                    if !self.replay && self.sources.get(&source).is_some_and(is_discord_bundle) && speakers::ax::is_trusted() {
                        for pid in speakers::ax::discord_processes() {
                            if let Ok(ax) =
                                speakers::ax::snapshot(pid, capture::host_now_ns() as f64 / 1e9)
                            {
                                ax_items.push(ax);
                            }
                        }
                    }
                    self.discord[&source]
                        .actor
                        .send(crate::speakers_runtime::Work::Observe(
                            t.clone(),
                            end_ns as f64 / 1e9,
                            text.clone(),
                            ax_items,
                        ))?;
                }
                let u = self.active.remove(&source).unwrap();
                let label = self
                    .sources
                    .get(&source)
                    .map_or_else(|| display_name_from_id(&source), display_name);
                let stamp = agent::now_ms();
                let observation = agent::protocol::Observation {
                    id: format!(
                        "{}:{}",
                        self.session.file_name().unwrap().to_string_lossy(),
                        u.id
                    ),
                    timestamp_ms: stamp,
                    source: label.clone(),
                    speaker: speaker.clone(),
                    language: language(&text).into(),
                    text: format!(
                        "{}{}",
                        text,
                        if u.notes.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", u.notes.join(" "))
                        }
                    ),
                };
                let mut segments = vec![ui::Segment::Words(text.clone())];
                segments.extend(
                    u.notes
                        .iter()
                        .map(|s| ui::Segment::Note(s.trim_matches(['[', ']']).into())),
                );
                let line = ui::TranscriptLine {
                    id: u.id,
                    label: Some(ui::Label {
                        source: label,
                        speaker: speaker.clone(),
                    }),
                    segments,
                    is_final: true,
                };
                if let Some(d)=self.discord.get(&source) {
                    let runs=self.call_word_runs(&all,&source,d.origin);
                    let mut lines:Vec<_>=runs.iter().map(|r|ui::TranscriptLine {id:u.id,label:Some(ui::Label {source:self.sources.get(&source).map_or_else(||display_name_from_id(&source),display_name),speaker:r.speaker.clone()}),segments:vec![ui::Segment::Words(r.text())],is_final:true}).collect();
                    if let Some(line)=lines.last_mut(){line.segments.extend(u.notes.iter().map(|s|ui::Segment::Note(s.trim_matches(['[',']']).into())));}
                    self.ui(ui::UiUpdate::TranscriptRuns {id:u.id,lines});
                } else {
                    self.ui(ui::UiUpdate::Transcript(line.clone()));
                }
                self.labels.insert(u.id, (line, track.clone()));
                if track.is_none() {
                    if let Some(d) = self.discord.get_mut(&source) {
                        d.unlabelled
                            .push((u.id, u.start, end_ns, text.clone(), speaker.clone()));
                        if d.unlabelled.len() > 2048 {
                            d.unlabelled.remove(0);
                        }
                    }
                }
                if self.labels.len() > 2048 {
                    if let Some(k) = self.labels.keys().copied().min() {
                        self.labels.remove(&k);
                    }
                }
                self.finals += 1;
                let audio_spans = vec![];
                let audio = self.audio.get(&source).context("missing final waveform").and_then(|ring| {
                    ensure!(!self.source_cutoffs.get(&source).is_some_and(|cutoff| u.start < *cutoff),
                        "final waveform unavailable across gap");
                    ensure!(end_ns <= ring.start + ring.samples.len() as u64 * 62500,
                        "final waveform end unavailable");
                    // The bounded ring intentionally keeps a tail of long utterances. Natural eviction
                    // is distinct from a gap; use exactly the still-retained, timestamp-aligned PCM.
                    let audio_start = u.start.max(end_ns.saturating_sub(30_000_000_000)).max(ring.start);
                    ring.clip(audio_start, end_ns)
                });
                // A capture gap can lose the waveform but not the words. The decider reads text only (decide/d1.rs) and
                // the agent gets text, so the final goes on as text and the gap is noted in its records (night review
                // L1: 20 Oct 18:28:19 "Uh whether we can run on" was archived but never reached the agent).
                let mut audio_error=None;
                let audio = audio.unwrap_or_else(|e| { audio_error=Some(format!("{e:#}")); vec![0.] });
                let pending_decision = serde_json::json!({"status":"pending_decision"});
                self.index.record(
                    &observation,
                    &source,
                    u.start,
                    end_ns,
                    &all,
                    &u.notes,
                    &pending_decision,
                )?;
                let guard = archive::DiskGuard {
                    live_floor: None,
                    dir: archive::store_root(&self.session),
                    floor: self.settings.free_space_floor_bytes,
                    status: self.disk_status.clone(),
                };
                if !guard.allows(8 * 1024 * 1024) {
                    self.errors += 1;
                    self.note_line("Archival and agent delivery stopped below disk floor; live transcript continues");
                    return Ok(());
                }
                let mut turn = decide::Turn {
                    id: observation.id.clone(),
                    role: if self.own_audio(&source) || wake_sound {
                        decide::Role::Agent
                    } else if unknown {
                        decide::Role::Other("Unknown".into())
                    } else if self.owner(&source) {
                        decide::Role::Owner
                    } else {
                        decide::Role::Other(speaker)
                    },
                    transcript: text.clone(),
                    audio: decide::AudioClip::new(audio)?,
                };
                // Some of his words were dictated to Vella: d1 and the agent read them marked (the archive keeps his words).
                if let Some(split) = self.vella_split(&source, &all) {
                    turn.transcript = split.marked;
                }
                if self.own_audio(&source) || wake_sound {
                    let mut record =
                        serde_json::json!({"status":"archive_only","given_to_agent":false,"would_wake":false});
                    if wake_sound { record["reason"] = serde_json::json!(if own_voice { "heimdalls_voice" } else { "wake_sound" }); }
                    if let Some(e) = &audio_error { record["audio_error"] = serde_json::json!(e); }
                    self.index.record(
                        &observation,
                        &source,
                        u.start,
                        end_ns,
                        &all,
                        &u.notes,
                        &record,
                    )?;
                    // Heimdall's final text enters through reply settlement.
                    // Its captured output is archive-only, never another ASR turn.
                    self.decisions += 1;
                    crate::diagnostics::event_stream(&serde_json::json!({"event":"final","dry_run":self.settings.dry_run,"id":observation.id,"source":observation.source,"speaker":observation.speaker,"text":observation.text,"decision":record,"would_wake":false}));
                    return Ok(());
                }
                let given = self.sources.get(&source).is_some_and(crate::context_policy::agent_context);
                if !given {
                    self.index.evidence.event("agent_context_routing", &source, std::slice::from_ref(&observation.id), end_ns,
                        serde_json::json!({"schema":1,"given_to_agent":false,"policy":"archive_and_search_only","observation":observation}))?;
                    let mut record = serde_json::json!({"status":"archive_only","given_to_agent":false,"would_wake":false});
                    if let Some(e) = &audio_error { record["audio_error"] = serde_json::json!(e); }
                    self.decisions += 1; // A completed archive-only disposition, without inference.
                    self.index.record(&observation, &source, u.start, end_ns, &all, &u.notes, &record)?;
                    crate::diagnostics::event_stream(&serde_json::json!({"event":"final","dry_run":self.settings.dry_run,"id":observation.id,"source":observation.source,"speaker":observation.speaker,"text":observation.text,"decision":record,"would_wake":false}));
                    return Ok(());
                }
                if self.is_call(&source) && !text.trim().is_empty() {
                    self.last_call_line_ns = Some(self.last_call_line_ns.map_or(end_ns, |t| t.max(end_ns)));
                    // speech-policy (night 3): the user is in a conversation; Heimdall keeps quiet (bridge/speech_policy.rs).
                    self.tools.speech_policy.other_line(end_ns);
                    if decide::send::farewell(&text) { self.call_goodbye(end_ns); }
                }
                self.call_final(u.id,&source,u.start,end_ns,&all,&u.notes);
                self.project_call(&source)?;
                if voice.is_some() { self.note_line_voice(&observation.id, if unknown { mic_voice::Line::Unknown } else { mic_voice::Line::Owner }); }
                let final_ = AgentFinal { audio_error, observation, source:source.clone(), start:u.start, end:end_ns, words:all, notes:u.notes, images:u.images, turn, audio_spans, forced, end_of_turn_probability };
                if self.is_call(&source) && track.is_none() {
                    // Never pass a mixed, undiarized call stream as if it belonged to a named speaker.
                    let status = if self.awaiting_speaker.len() < 64 { "awaiting_diarization" } else { "diarization_backlog_full" };
                    let record = serde_json::json!({"status":status,"given_to_agent":false,"would_wake":false});
                    self.index.record(&final_.observation,&source,final_.start,end_ns,&final_.words,&final_.notes,&record)?;
                    self.index.evidence.event("agent_context_routing",&source,std::slice::from_ref(&final_.observation.id),end_ns,
                        serde_json::json!({"schema":1,"given_to_agent":false,"policy":status,"observation":final_.observation}))?;
                    if self.awaiting_speaker.len() < 64 { self.awaiting_speaker.insert(u.id,final_); }
                    else { self.component_status("call-context",Some("Call context waiting on unavailable speaker labels; retained in archive".into())); }
                } else {
                    if let Some((_permit,through))=self.early_sends.remove(&final_.observation.id) {
                        // The words went out before command inference. A screenshot that arrives now steers the
                        // running turn or waits for the next wake; it never wakes the agent by itself (E9.1). Another
                        // microphone's copy of an utterance (`heard_twice`) carries nothing more.
                        if !final_.images.is_empty() && !self.heard_twice.copies.contains(&final_.observation.id) {
                            if self.dispatched_through<through && self.tools.send_context.lock().unwrap().has_pending_through(through) {
                                // Its words are still waiting for their own dispatch, which carries the screenshot.
                                let captured=capture::host_now_ns();
                                self.attachment_stamps.extend(std::iter::repeat_n((through,captured),final_.images.len()));
                                self.attachment_batch.extend(final_.images);
                            } else if let Some(agent)=&self.agent {
                                if let Ok(permit)=agent.wake_permit(self.settings.dry_run) {
                                    let caption=late_screenshot_caption(&final_.observation.text, final_.observation.timestamp_ms);
                                    agent.late_images_with_permit(caption,final_.images,permit)?;
                                }
                            }
                        }
                    } else if dictated {self.give_dictated(final_)?;} else {self.give_to_agent(final_)?;}
                }
            }
        }
        Ok(())
    }
    pub(super) fn admit_before_command(&mut self, event: &ListenEvent) -> Result<()> {
        let ListenEvent::FinalUtterance {
            source,
            words,
            start_ns,
            end_ns,
            forced,
            end_of_turn_probability,
            ..
        } = event
        else {
            return Ok(());
        };
        if !self.owner(source) {
            return Ok(());
        }
        let queued_start = self.command_deferred.get(source).and_then(|events| {
            events.iter().rev().find_map(|e| match e {
                ListenEvent::SpeechStart { host_time_ns, .. } if *host_time_ns <= *start_ns => {
                    Some(*host_time_ns)
                }
                _ => None,
            })
        });
        let separate =
            queued_start.filter(|start| self.active.get(source).is_none_or(|u| *start > u.start));
        let mut all = if let Some(start) = separate {
            self.command_deferred
                .get(source)
                .into_iter()
                .flat_map(|events| events.iter())
                .filter_map(|e| match e {
                    ListenEvent::TranscriptSegment {
                        start_ns, words, ..
                    } if *start_ns >= start => Some(words.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        } else {
            self.active
                .get(source)
                .map(|u| u.prefix.clone())
                .unwrap_or_default()
        };
        all.extend(words.clone());
        // Heimdall's own sound on his microphone (the wake sound, an acknowledgement) is never sent early; the final
        // archives it (bridge/speech_policy.rs).
        if self.heard_own_sound(source, &all, *start_ns, *end_ns) {
            return Ok(());
        }
        // Echo guard (bug notes 23 Oct 17:34 and 17:36): an early send carries the words the final keeps, never the
        // echo the guard left out. The follow-up window sent the whole echoed reply here before the final filtered it,
        // and an echo of a command or of Heimdall's name must not make this send happen at all.
        let echo = self.echo_verdict(source, &all).filter(|v| v.any());
        if let Some(v) = &echo {
            if v.own() {
                // A sentence registered since `heard_own_sound` made it Heimdall's after all: the final archives it.
                return Ok(());
            }
        }
        let heard_words = all.clone();
        if let Some(v) = &echo {
            all = v.kept(&all);
        }
        let text = listen::Transcript::from_words(all.clone()).text;
        // Words dictated to Vella never send early: only his words outside dictation are matched (pipeline/vella.rs).
        let split = self.vella_split(source, &all);
        let trigger_words = split.as_ref().map_or_else(|| all.clone(), |s| s.kept.clone());
        let trigger_text = listen::Transcript::from_words(trigger_words.clone()).text;
        let timed: Vec<_> = trigger_words.iter().map(screen::TimedWord::from).collect();
        self.tools.speech_status();
        #[cfg(test)]
        if !self.command_test_answers.is_empty() {
            return Ok(());
        }
        let has_command = self.command_inflight.get(source).copied().unwrap_or(0) > 0
            || self.command_candidates.iter().any(|c| c.source == *source)
            || !screen::PhraseMatcher::new(COMMAND_PHRASES)
                .observe_utterance(source, "explicit", &timed, true)
                .is_empty();
        let explicit = !trigger_words.is_empty() && decide::send::explicit_trigger(
            &trigger_text,
            true,
            *end_ns,
            self.tools.send_context.lock().unwrap().finished_ns,
        )
        .is_some();
        if !has_command && !explicit {
            return Ok(());
        }
        // Only his own voice sends early (mic_voice.rs): Heimdall's or another voice waits for the final, which archives
        // or labels it. The final reads this same verdict.
        if self.primary_mic(source) {
            let start = separate.or_else(|| self.active.get(source).map(|u| u.start)).unwrap_or(*start_ns);
            let utterance = self.active.get(source).filter(|_| separate.is_none()).map(|u| format!("{}:{}", self.session.file_name().unwrap().to_string_lossy(), u.id)).unwrap_or_default();
            let voice = self.mic_voice(source, &utterance, start, *end_ns, &heard_words, echo.as_ref()).voice;
            if matches!(voice, heimdall_voice::MicVoice::Heimdall | heimdall_voice::MicVoice::Unknown) {
                return Ok(());
            }
        }
        self.ensure_utterance(source, *start_ns);
        let existing = &self.active[source];
        let (id_number, notes, images) = if let Some(start) = separate {
            let id = *self
                .early_ids
                .entry((source.clone(), start))
                .or_insert_with(|| {
                    let id = self.next_id;
                    self.next_id += 1;
                    id
                });
            (id, vec![], vec![])
        } else {
            (existing.id, existing.notes.clone(), existing.images.clone())
        };
        let id = format!(
            "{}:{}",
            self.session.file_name().unwrap().to_string_lossy(),
            id_number
        );
        if self.early_sends.contains_key(&id) {
            return Ok(());
        }
        // Admit text and any pure trigger before submitting this final's command
        // question. An unrelated in-flight model/lock never enters this path.
        let clip = self
            .audio
            .get(source)
            .context("waveform unavailable")
            .and_then(|ring| {
                ring.clip(
                    (*start_ns)
                        .max(end_ns.saturating_sub(30_000_000_000))
                        .max(ring.start),
                    *end_ns,
                )
            });
        let (audio, audio_error) = match clip {
            Ok(a) => (a, None),
            Err(e) => (vec![0.], Some(format!("{e:#}"))),
        };
        let observation = agent::protocol::Observation {
            id: id.clone(),
            timestamp_ms: agent::now_ms(),
            source: self
                .sources
                .get(source)
                .map_or_else(|| display_name_from_id(source), display_name),
            speaker: "User".into(),
            language: language(&text).into(),
            text: if notes.is_empty() {
                text.clone()
            } else {
                format!("{} {}", text, notes.join(" "))
            },
        };
        let final_ = AgentFinal {
            audio_error,
            observation,
            source: source.clone(),
            start: *start_ns,
            end: *end_ns,
            words: all,
            notes,
            images,
            turn: decide::Turn {
                id: id.clone(),
                role: decide::Role::Owner,
                transcript: split.map_or(text, |s| s.marked),
                audio: decide::AudioClip::new(audio)?,
            },
            audio_spans: vec![],
            forced: *forced,
            end_of_turn_probability: *end_of_turn_probability,
        };
        // The same permission survives an Off/On transition; it cannot reacquire.
        let admission = self.admit_agent_final(final_)?;
        if separate.is_none() {
            self.active.get_mut(source).unwrap().images.clear();
        }
        self.early_sends.insert(id, admission);
        Ok(())
    }
    pub(super) fn drain_command_events(&mut self, source: &str) -> Result<()> {
        if self.command_inflight.get(source).copied().unwrap_or(0) > 0 {
            return Ok(());
        }
        while let Some(e) = self
            .command_deferred
            .get_mut(source)
            .and_then(|q| q.pop_front())
        {
            self.listen(e)?;
            if self.command_inflight.get(source).copied().unwrap_or(0) > 0 {
                break;
            }
        }
        Ok(())
    }
    pub(super) fn give_to_agent(&mut self, final_: AgentFinal) -> Result<()> {
        self.admit_agent_final(final_).map(drop)
    }
    pub(super) fn admit_agent_final(
        &mut self,
        final_: AgentFinal,
    ) -> Result<(Option<agent::delivery::DeliveryPermit>, u64)> {
        // One utterance heard on two microphones reaches the decider and the agent once (`heard_twice`).
        if self.archive_heard_twice(&final_)? {
            return Ok((None, 0));
        }
        let AgentFinal {
            audio_error,
            observation,
            source,
            start,
            end: end_ns,
            words: all,
            notes,
            images,
            turn,
            audio_spans,
            forced,
            end_of_turn_probability,
        } = final_;
        if !self.replay { crate::latency::admitted(&turn.id, &source, end_ns, forced); } // Latency map (lab/latency).
        let archive_observation = observation.clone();
        let mut observation = observation;
        observation.source = self.sources.get(&source).map_or_else(|| display_name_from_id(&source), agent_source_name);
        if turn.transcript != listen::Transcript::from_words(all.clone()).text {
            observation.text = turn.transcript.clone();
            observation.language = language(&observation.text).into();
        }
        if let Some(runs)=self.call_final_runs(&observation.id,&all,&source) {
            (observation.speaker,observation.text)=speech_split::context_text(&runs);
        }
        // What the user dropped onto the notch while saying this (or just before he started) is part of this turn: his
        // words, then the drop's lines, its images attached, one wake (`drops.rs`).
        let mut images = images;
        // Only his spoken words count for `listen_to`, never the drop's lines (a guest's "listen to Vim" he dragged in).
        let spoken = observation.text.clone();
        // Words dictated to Vella trigger nothing: his name, "send it", the follow-up window, a stop phrase and
        // `listen_to` read only his words outside dictation (pipeline/vella.rs). Unchanged when none was dictated.
        let (spoken, trigger_text, _stop_words) = match self.vella_split(&source, &all) {
            Some(split) => { let t = listen::Transcript::from_words(split.kept.clone()).text; (t.clone(), Some(t), Some(split.kept)) }
            None => (spoken, None, None),
        };
        let joined = if self.owner_line(&source, &observation.id) && !self.own_audio(&source) { self.drops.take_for_turn() } else { vec![] };
        for m in &joined {
            observation.text.push('\n');
            observation.text.push_str(&m.with_speech());
            images.extend(m.images.iter().cloned());
            self.index.evidence.event("notch_drop_delivery", "notch", std::slice::from_ref(&observation.id), end_ns, serde_json::json!({"label":m.label,"lines":m.lines,"images":m.images.len(),"joined":true}))?;
        }
        // Listening to guests (listen_to.rs): a guest the user listens to who addresses Heimdall by name wakes it; the
        // agent's copy marks their lines. d1's conversation keeps the plain labels it learned on.
        let guest = self.guest_address(&source, &observation, &all);
        // A listened guest's own line counts like his ("if I say 'listen to Kanan', Kanan should be
        // treated like me"): their mention of the name, "send it" and d1's verdict follow his rules for a call.
        if let Some(name) = self.listened_line(&source, &observation, &all) {
            self.note_line_voice(&observation.id, mic_voice::Line::Guest(name));
        }
        let mut for_agent = observation.clone();
        self.mark_listened(&mut for_agent);
        if self.agent.is_none() {
            self.agent = Some(AgentRuntime::start(
                self.settings.clone(),
                self.data.clone(),
                self.session.clone(),
                self.tools.clone(),
                self.ui,
            )?);
        }
        // The suggester's window of talk (suggest.rs): the user's own finished line may start a pass.
        let owner = self.owner_line(&source, &observation.id) && !self.own_audio(&source);
        self.suggest_heard(&observation.speaker, &observation.text, owner);
        self.agent
            .as_ref()
            .unwrap()
            .send(Command::Observe(for_agent.clone()))?;
        let mut routing = serde_json::json!({"schema":1,"given_to_agent":true,"policy":"microphone_or_call","observation":for_agent,"audio_spans":audio_spans});
        if let Some(g) = &guest { routing["listened_guest"] = serde_json::json!(g); }
        self.index.evidence.event("agent_context_routing", &source, std::slice::from_ref(&observation.id), end_ns, routing)?;
        // One text conversation includes microphone/call finals, typed barriers,
        // and settled final answers. Keep full pending text for delivery.
        self.tools.speech_status();
        // Cards on screen, read before the conversation lock (bridge/questions.rs).
        let card = if self.owner_line(&source, &observation.id) { self.tools.card_state(start, end_ns) } else { crate::bridge::questions::CardState::None };
        let (snapshot, heard_trigger) = {
            let mut context = self.tools.send_context.lock().unwrap();
            // The start lets a wake tell speech that began before a card click from speech after it (bridge/questions.rs).
            let mut observed = serde_json::to_value(&observation)?;
            observed["start_ns"] = serde_json::json!(start);
            context.observe_at(
                &observation.speaker,
                &observation.text,
                end_ns,
                observed,
            );
            let trigger = decide::send::explicit_trigger(
                trigger_text.as_deref().unwrap_or(&observation.text),
                self.like_owner(&source, &observation.id),
                end_ns,
                context.finished_ns,
            );
            // While a question card is answering, the follow-up window does not bypass d1 (bridge/questions.rs).
            let trigger = crate::bridge::questions::card_trigger(trigger, card);
            // A listened guest's address by name is their request (`Trigger::Guest`), even where their line also
            // counts like his.
            let trigger = guest.as_ref().map(|_| decide::send::Trigger::Guest).or(trigger);
            self.tools.persist_send_context(&context)?;
            (context.snapshot(), trigger)
        };
        // His microphone's words, by source (never a label that reads "the user"), for `listen_to` (listen_to.rs).
        if self.owner_line(&source, &observation.id) { self.owner_heard(snapshot.through, &spoken); }
        // Night review D3: during a call, the follow-up window, "send it" and a mention of the name wait for d1, which
        // holds them back when it says strongly that the line is part of the call (decide::send::CALL_NOT_FOR). The user's
        // own address by name still wakes at once.
        // His goodbye in a call ends it too ("See ya, sir", 29 Oct 22:27), and so does his "I'm off the call" or "you can
        // talk to me now" (bug 2026-11-04T17-48-24).
        if self.owner_line(&source, &observation.id) && self.in_call_at(end_ns)
            && (decide::send::farewell(&observation.text) || decide::send::off_call(&observation.text)) { self.call_goodbye(end_ns); }
        let in_call = self.like_owner(&source, &observation.id) && self.in_call_at(end_ns);
        let call_trigger = heard_trigger.filter(|t| joined.is_empty() && in_call && decide::send::call_trigger_waits(*t, &observation.text));
        // A drop with his words is for Heimdall, in a call or not.
        let explicit = if joined.is_empty() { heard_trigger.filter(|_| call_trigger.is_none()) } else { Some(decide::send::Trigger::Dropped) };
        self.pending_text = self
            .tools
            .send_context
            .lock()
            .unwrap()
            .pending_text()
            .lines()
            .map(str::to_owned)
            .collect();
        let captured = capture::host_now_ns();
        self.attachment_stamps
            .extend(std::iter::repeat_n((snapshot.through, captured), images.len()));
        self.attachment_batch.extend(images);
        ensure!(
            self.attachment_batch.len() <= 64,
            "pending screenshot attachment bound exceeded"
        );
        // Archiving a final never waits for model inference. Search and a process
        // restart can see it while its decision is still queued.
        let mut pending = serde_json::json!({
            "schema":1,"status":"decision_pending","given_to_agent":true,"would_wake":explicit.is_some(),"trigger":explicit
        });
        if let Some(t) = call_trigger { pending["call_trigger"] = serde_json::json!(t); }
        // Delivered as text with its waveform lost to a capture gap (L1): the gap stays visible in the archive and log.
        if let Some(error) = &audio_error { pending["audio_error"] = serde_json::json!(error); }
        self.index.record(&archive_observation,&source,start,end_ns,&all,&notes,&pending)?;
        #[cfg(test)]
        let injected_send = !self.hooks.send.is_empty() || !self.hooks.send_choices.is_empty() || !self.hooks.send_probs.is_empty() || !self.hooks.send_v3.is_empty();
        // Capture authority at admission. Explicit cues queue a wake before any
        // native work; a stale result cannot reacquire an Off/On generation.
        let permit = self
            .agent
            .as_ref()
            .unwrap()
            .wake_permit(self.settings.dry_run)
            .ok();
        if explicit.is_some() && !self.own_audio(&source) {
            // His name or "send it" is for Heimdall, so it may close a card; the follow-up window alone is not
            // (bridge/questions.rs).
            let for_heimdall = explicit.is_some_and(crate::bridge::questions::for_heimdall);
            let speaker = if self.owner_line(&source, &observation.id) { agent::Speaker::Owner } else { agent::Speaker::Other };
            let before = self.dispatched_through;
            let ack = self.ack_plan_for(None, trigger_text.as_deref().unwrap_or(&observation.text));
            self.dispatch_pending(&source, &observation.text, snapshot.through, &observation.id, speaker, permit.clone(), for_heimdall, ack)?;
            // speech-policy (night 3): the user addressing Heimdall by name may be answered aloud during a conversation,
            // in the run this wake starts or steers (bridge/speech_policy.rs).
            if self.dispatched_through > before && explicit == Some(decide::send::Trigger::Name) && self.like_owner(&source, &observation.id)
                && decide::send::addressed_by_name(trigger_text.as_deref().unwrap_or(&observation.text)) {
                self.tools.speech_policy.addressed_wake();
            }
            // A listened guest addressed Heimdall by name: it may answer them aloud during the call, as it may the user.
            if self.dispatched_through > before && explicit == Some(decide::send::Trigger::Guest) {
                self.tools.speech_policy.addressed_wake();
            }
        }
        let mut audio = serde_json::json!({"utterance_id":turn.id,"source_id":source,"start_ns":end_ns.saturating_sub(turn.audio.samples.len() as u64*62500).max(start),"end_ns":end_ns,"rate":16000,"samples":turn.audio.samples.len(),"pcm_sha256":heimdall_archive::hash(&heimdall_archive::audio::pcm_bytes(&turn.audio.samples)),"spans":audio_spans});
        if let Some(error) = &audio_error {
            audio = serde_json::json!({"utterance_id":turn.id,"source_id":source,"unavailable":error,"spans":audio_spans});
        }
        if !audio_spans.is_empty() {
            audio["start_ns"] = audio_spans.first().unwrap()["start_ns"].clone();
            audio["end_ns"] = audio_spans.last().unwrap()["end_ns"].clone();
        }
        // Preserve the original archive text while the model gets any routed suffix.
        let job = send_worker::PendingSend {
            final_: AgentFinal {
                audio_error,
                observation: archive_observation,
                source: source.clone(),
                start,
                end: end_ns,
                words: all,
                notes,
                images: vec![],
                turn: turn.clone(),
                audio_spans,
                forced,
                end_of_turn_probability,
            },
            snapshot: snapshot.clone(),
            explicit,
            call_trigger,
            permit: permit.clone(),
            audio,
            admitted: Instant::now(),
        };
        #[cfg(test)]
        let injected = (|| -> Option<send_worker::Answer> {
            if !injected_send {
                return None;
            }
            Some((|| -> send_worker::Answer {
                #[cfg(test)]
                if let Some(ack) = self.hooks.send_v3.pop_front() {
                    let fit = snapshot.fit(|text| Ok(text.len() / 4))?;
                    let probabilities = [("send".into(), 1.), ("wait".into(), 0.), ("not_for_heimdall".into(), 0.)].into();
                    return Ok((fit, decide::send::Prediction { choice: decide::send::Choice::Send, probabilities, input_tokens: 0,
                        output: serde_json::json!({"test":true}), ack }));
                }
                #[cfg(test)]
                if let Some(choice) = self.hooks.send_choices.pop_front() {
                    let fit = snapshot.fit(|text| Ok(text.len() / 4))?;
                    let probabilities = [
                        decide::send::Choice::Send,
                        decide::send::Choice::Wait,
                        decide::send::Choice::NotForHeimdall,
                    ]
                    .into_iter()
                    .map(|c| (c.key().into(), if c == choice { 1. } else { 0. }))
                    .collect();
                    return Ok((
                        fit,
                        decide::send::Prediction {
                            choice,
                            probabilities,
                            input_tokens: 0,
                            output: serde_json::json!({"test":true}),
                            ack: None,
                        },
                    ));
                }
                #[cfg(test)]
                if let Some((send, wait, not_for)) = self.hooks.send_probs.pop_front() {
                    let fit = snapshot.fit(|text| Ok(text.len() / 4))?;
                    let choice = if send >= wait && send >= not_for { decide::send::Choice::Send }
                        else if not_for >= wait { decide::send::Choice::NotForHeimdall } else { decide::send::Choice::Wait };
                    let probabilities = [("send".into(), send), ("wait".into(), wait), ("not_for_heimdall".into(), not_for)].into();
                    return Ok((fit, decide::send::Prediction { choice, probabilities, input_tokens: 0, output: serde_json::json!({"test":true}), ack: None }));
                }
                #[cfg(test)]
                if let Some(p) = self.hooks.send.pop_front() {
                    let fit = snapshot.fit(|text| Ok(text.len() / 4))?;
                    let choice = if p >= 0.5 {
                        decide::send::Choice::Send
                    } else {
                        decide::send::Choice::Wait
                    };
                    return Ok((
                        fit,
                        decide::send::Prediction {
                            choice,
                            probabilities: [
                                ("send".into(), p),
                                ("wait".into(), 1. - p),
                                ("not_for_heimdall".into(), 0.),
                            ]
                            .into(),
                            input_tokens: 0,
                            output: serde_json::json!({"test":true}),
                            ack: None,
                        },
                    ));
                }
                unreachable!()
            })())
        })();
        #[cfg(test)]
        if let Some(result) = injected {
            if std::mem::take(&mut self.hooks.talk_before_verdict) {
                self.listen(ListenEvent::SpeechStart { source: source.clone(), host_time_ns: end_ns + 1 })?;
            }
            self.complete_send(send_worker::CompletedSend {
                job,
                result,
                prompt: None,
            })?;
            self.context.push_back(turn);
            while self.context.len() > 20 {
                self.context.pop_front();
            }
            return Ok((permit, snapshot.through));
        }
        // Retain full sentence ownership in the completion even on saturation.
        if self.inference()?.send_full() {
            self.complete_send(send_worker::CompletedSend {
                job,
                result: Err(anyhow::anyhow!(
                    "send decision backlog full; sentence retained"
                )),
                prompt: None,
            })?;
        } else {
            self.inference()?.submit(job)?;
        }
        self.context.push_back(turn);
        while self.context.len() > 20 {
            self.context.pop_front();
        }
        Ok((permit, snapshot.through))
    }
    pub(super) fn ensure_utterance(&mut self, s: &str, ns: u64) {
        if !self.active.contains_key(s) {
            let id = self.next_id;
            self.next_id += 1;
            self.active.insert(
                s.into(),
                Utterance {
                    id,
                    start: ns,
                    ..Default::default()
                },
            );
        }
    }
    pub(super) fn archive_waiting_calls(&mut self, source: &str, reason: &str) {
        let ids: Vec<_> = self
            .awaiting_speaker
            .iter()
            .filter(|(_, f)| f.source == source)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let f = self.awaiting_speaker.remove(&id).unwrap();
            let record =
                serde_json::json!({"status":reason,"given_to_agent":false,"would_wake":false});
            let result = (|| -> Result<()> {
                self.index.record(
                    &f.observation,
                    source,
                    f.start,
                    f.end,
                    &f.words,
                    &f.notes,
                    &record,
                )?;
                self.index.evidence.event("agent_context_routing",source,std::slice::from_ref(&f.observation.id),f.end,serde_json::json!({"schema":1,"given_to_agent":false,"policy":reason,"observation":f.observation}))
            })();
            if let Err(e) = result {
                eprintln!("Archive undelivered call: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::late_screenshot_caption;
    /// E9.1 (18 Oct 17:44:56): the caption names the words and their time, so the steered screenshot is attributable.
    #[test]
    fn a_late_screenshot_caption_names_its_words_and_their_clock_time() {
        let said = 1_792_338_296_000; // 17:44:56 CEST on 18 Oct 2026
        assert_eq!(late_screenshot_caption("look at the screen right now", said),
            format!("Screenshot for \"look at the screen right now\" ({})", agent::clock::local_hms(said)));
        assert!(late_screenshot_caption("look", said).ends_with(&format!("({})", agent::clock::local_hms(said))));
    }
}
