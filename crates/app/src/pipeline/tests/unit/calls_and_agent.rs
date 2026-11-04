    use super::*;
    #[test]
    fn discord_initialization_is_async_and_stale_failures_are_ignored() {
        let (_t, mut a) = app();
        let info = crate::fake_source("discord", 0, 1);
        let source = info.id.0.clone();
        a.source(info);
        assert!(a.discord.is_empty());
        a.audio.insert(
            source.clone(),
            AudioRing {
                start: 1_000_000_000,
                samples: VecDeque::from(vec![0.1; 320]),
            },
        );
        a.listen(ListenEvent::SpeechStart {
            source: source.clone(),
            host_time_ns: 1_000_000_000,
        })
        .unwrap();
        assert!(a.discord[&source].loading);
        let generation = a.discord[&source].actor_generation;
        a.speaker_update(crate::speakers_runtime::Event {
            source: source.clone(),
            generation,
            update: crate::speakers_runtime::Update::Error("worker crashed".into()),
        })
        .unwrap();
        assert!(a.failure.is_none());
        assert!(a.discord.contains_key(&source));
        assert!(a.audio.contains_key(&source));
        assert!(a.active.contains_key(&source));
        assert_eq!(a.gaps, 0);
        assert!(!a.discord[&source].active);
        assert!(a.discord[&source].loading);
        a.speaker_update(crate::speakers_runtime::Event { source:source.clone(),generation,update:crate::speakers_runtime::Update::GpuStatus(None) }).unwrap();
        assert!(a.component_status.contains_key(&format!("speakers:{source}")));
        let errors = a.errors;
        a.speaker_update(crate::speakers_runtime::Event {
            source: source.clone(),
            generation: generation + 1,
            update: crate::speakers_runtime::Update::Error("stale".into()),
        })
        .unwrap();
        assert_eq!(a.errors, errors);
        assert!(a.source_open(&source));
    }
    #[test]
    fn coded_voices_label_lines_without_a_prompt_and_relabel_archived_utterances() {
        let (_t, mut a) = app();
        let track = TrackId {
            source: "discord".into(),
            session: 1,
            speaker: 2,
        };
        // A new voice on a call: a code label, counted, and nothing for the user to answer.
        a.speaker_events(vec![SpeakerEvent::VoiceCoded { track: track.clone(), code: "Speaker Fri 23 Oct 21h #1".into(), new: true }])
            .unwrap();
        assert_eq!((a.coded_voices, a.speaker_prompts), (1, 0));
        assert_eq!(a.speaker_names[&track], "Speaker Fri 23 Oct 21h #1");
        assert!(!a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::AgentNotice(_) | ui::UiUpdate::VoiceNameEntry { .. })));
        let id = 21;
        let o = agent::protocol::Observation {
            id: format!("{}:{id}", a.session.file_name().unwrap().to_string_lossy()),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: "Speaker 2".into(),
            language: "en".into(),
            text: "hello".into(),
        };
        a.index
            .record(&o, "discord", 1, 2, &[], &[], &serde_json::json!({}))
            .unwrap();
        a.labels.insert(
            id,
            (
                ui::TranscriptLine {
                    id,
                    label: Some(ui::Label {
                        source: "Discord".into(),
                        speaker: "Speaker 2".into(),
                    }),
                    segments: vec![],
                    is_final: true,
                },
                Some(track.clone()),
            ),
        );
        a.speaker_events(vec![SpeakerEvent::LabelChanged {
            track,
            name: "Sam".into(),
        }])
        .unwrap();
        assert_eq!(a.labels[&id].0.label.as_ref().unwrap().speaker, "Sam");
        let c = heimdall_archive::store::open(&crate::archive::store_root(&a.session)).unwrap();
        let name: String = c
            .query_row("SELECT speaker FROM utterances", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Sam");
    }
    #[test]
    fn non_call_app_audio_is_searchable_but_never_enters_agent_or_d1_context() {
        let (_t, mut a) = app();
        let i = crate::fake_source("app", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.audio.insert(
            source.clone(),
            AudioRing {
                start: 0,
                samples: vec![0.01; 16000 * 10].into(),
            },
        );
        say(
            &mut a,
            &source,
            "random video take a screenshot",
            2_000_000_000,
            false,
        );
        assert!(a.agent.is_none());
        assert!(a.context.is_empty());
        assert!(a.pending_text.is_empty());
        assert!(a.send_worker.as_ref().is_none_or(|o|o.outstanding.load(Ordering::Acquire)==0));
        assert_eq!(a.errors, 0);
        let rows = a.index.utterances_between(0, 10_000_000_000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["decision"]["status"], "archive_only");
        let records = a.index.evidence.records().unwrap();
        assert!(records
            .iter()
            .any(|r| r.kind == "agent_context_routing" && r.payload["given_to_agent"] == false));
        assert!(!records
            .iter()
            .any(|r| r.kind == "command_decision" || r.kind == "d1_question"));
    }
    #[test]
    fn call_final_waits_then_delivers_with_diarized_name() {
        let (_t, mut a) = app();
        let i = crate::fake_source("discord", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.audio.insert(
            source.clone(),
            AudioRing {
                start: 0,
                samples: vec![0.01; 16000 * 10].into(),
            },
        );
        say(&mut a, &source, "hello from the call", 2_000_000_000, false);
        assert_eq!(a.awaiting_speaker.len(), 1);
        assert!(a.agent.is_none());
        assert!(a.pending_text.is_empty());
        let session = a.discord[&source].session;
        a.speaker_names.insert(
            TrackId {
                source: source.clone(),
                session,
                speaker: 2,
            },
            "Alex".into(),
        );
        a.discord.get_mut(&source).unwrap().segments = vec![speakers::diar::SpeakerSegment {
            start_time: 0.,
            end_time: 10.,
            speaker: 2,
        }];
        a.relabel_discord(&source).unwrap();
        assert!(a.awaiting_speaker.is_empty());
        assert!(a
            .pending_text
            .iter()
            .any(|t| t.contains("Alex") && t.contains("hello from the call")));
        assert!(
            matches!(&a.context.back().unwrap().role,decide::Role::Other(name) if name=="Alex")
        );
        let records = a.index.evidence.records().unwrap();
        assert!(records.iter().any(|r| r.kind == "agent_context_routing"
            && r.payload["given_to_agent"] == true
            && r.payload["observation"]["speaker"] == "Alex"));
    }
    #[test]
    fn late_mixed_call_labels_keep_two_rendered_and_archived_runs() {
        let (_temp,mut a)=app();let source_info=crate::fake_source("discord",0,1);let source=source_info.id.0.clone();a.source(source_info);
        a.audio.insert(source.clone(),AudioRing {start:0,samples:vec![0.01;16000*10].into()});
        say(&mut a,&source,"first full long sentence second full long sentence",2_000_000_000,false);
        let origin=a.discord[&source].origin;let session=a.discord[&source].session;
        let rows=a.index.utterances_between(0,10_000_000_000).unwrap();
        let words:Vec<Word>=serde_json::from_value(rows[0]["words"].clone()).unwrap();assert_eq!(words.len(),8);
        let middle=words[3].end_ns as f64/1e9-origin as f64/1e9;
        a.discord.get_mut(&source).unwrap().segments=vec![speakers::diar::SpeakerSegment {speaker:1,start_time:0.,end_time:middle},speakers::diar::SpeakerSegment {speaker:2,start_time:middle,end_time:10.}];
        for (speaker,name) in [(1,"Ada"),(2,"Bob")] {a.speaker_names.insert(TrackId {source:source.clone(),session,speaker},name.into());}
        a.relabel_discord(&source).unwrap();a.project_call(&source).unwrap();
        let rows=a.index.utterances_between(0,10_000_000_000).unwrap();assert_eq!(rows.len(),2);assert_eq!(rows[0]["speaker"],"Ada");assert_eq!(rows[1]["speaker"],"Bob");
        let updates=a.hooks.ui.lock().unwrap();
        let last=updates.iter().rev().find(|u|matches!(u,ui::UiUpdate::Transcript(_)|ui::UiUpdate::TranscriptRuns {..})).unwrap();
        assert!(matches!(last,ui::UiUpdate::TranscriptRuns {lines,..} if lines.len()==2&&lines[0].label.as_ref().unwrap().speaker=="Ada"&&lines[1].label.as_ref().unwrap().speaker=="Bob"));
    }
    #[test]
    fn pending_batch_never_wakes_from_silence_or_resumed_voice() {
        let (_t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let id = i.id.0.clone();
        a.source(i);
        a.live_worker = Some(Arc::new(
            listen::ListenWorker::spawn(Default::default(), || {
                ListenEngine::new(Silent, Silent, Silent, Default::default())
            })
            .unwrap(),
        ));
        a.agent = Some(
            AgentRuntime::start(
                a.settings.clone(),
                a.data.clone(),
                a.session.clone(),
                a.tools.clone(),
                None,
            )
            .unwrap(),
        );
        let o = agent::protocol::Observation {
            id: "pending".into(),
            timestamp_ms: 1,
            source: "Mic".into(),
            speaker: "User".into(),
            language: "en".into(),
            text: "Finish this thought.".into(),
        };
        a.pending_text.push_back(o.labelled());
        a.agent.as_ref().unwrap().send(Command::Observe(o)).unwrap();
        a.listen(ListenEvent::SpeechEnd {
            source: id.clone(),
            host_time_ns: 1_000_000_000,
        })
        .unwrap();
        a.tools.voice(&id, true); // VAD forwarder notices resumption while main is busy.
        let push = |a: &mut Pipeline, ns| {
            a.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {
                source_id: capture::SourceId(id.clone()),
                label: "mic".into(),
                host_ns: ns,
                samples: vec![0.; 320],
                discontinuity: false,
            }))
            .unwrap()
        };
        push(&mut a, 3_500_000_000);
        assert!(!a.pending_text.is_empty());
        a.listen(ListenEvent::SpeechStart {
            source: id.clone(),
            host_time_ns: 3_600_000_000,
        })
        .unwrap();
        assert!(!a.quiet_since.contains_key(&id));
        for i in 1..16 {
            push(&mut a, 3_500_000_000 + i * 20_000_000);
        }
        a.listen(ListenEvent::SpeechEnd {
            source: id.clone(),
            host_time_ns: 3_800_000_000,
        })
        .unwrap();
        for i in 1..125 {
            push(&mut a, 3_800_000_000 + i * 20_000_000);
        }
        assert!(!a.pending_text.is_empty());
        push(&mut a, 6_300_000_000);
        assert!(!a.pending_text.is_empty());
        a.agent.as_ref().unwrap().barrier().unwrap();
        let log = std::fs::read_to_string(crate::archive::run_dir(&a.data, &a.session).join("dry-run.jsonl")).unwrap();
        let records: Vec<serde_json::Value> = log
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["would_send"]["shouldQuery"], false);
    }
    #[test]
    fn external_silence_never_consumes_pending_without_positive_decision() {
        for source in ["discord", "app"] {
            let (_t, mut a) = app();
            let info = crate::fake_source(source, 1, 1);
            let id = info.id.0.clone();
            a.source(info);
            a.live_worker = Some(Arc::new(
                listen::ListenWorker::spawn(Default::default(), || {
                    ListenEngine::new(Silent, Silent, Silent, Default::default())
                })
                .unwrap(),
            ));
            a.agent = Some(
                AgentRuntime::start(
                    a.settings.clone(),
                    a.data.clone(),
                    a.session.clone(),
                    a.tools.clone(),
                    None,
                )
                .unwrap(),
            );
            let observation = agent::protocol::Observation {
                id: "external-pending".into(),
                timestamp_ms: 1,
                source: source.into(),
                speaker: "Other".into(),
                language: "en".into(),
                text: "Conversation for context.".into(),
            };
            a.pending_text.push_back(observation.labelled());
            a.agent
                .as_ref()
                .unwrap()
                .send(Command::Observe(observation))
                .unwrap();
            // Defend even against an already armed timer from older state.
            a.quiet_since.insert(id.clone(), 1_000_000_000);
            for ns in [3_500_000_000, 61_000_000_000] {
                a.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {
                    source_id: capture::SourceId(id.clone()),
                    label: source.into(),
                    host_ns: ns,
                    samples: vec![0.; 320],
                    discontinuity: false,
                }))
                .unwrap();
                assert!(!a.pending_text.is_empty());
            }
            a.agent.as_ref().unwrap().barrier().unwrap();
            let log = std::fs::read_to_string(crate::archive::run_dir(&a.data, &a.session).join("dry-run.jsonl")).unwrap();
            assert_eq!(log.lines().count(), 1, "external content is append-only");
            let record: serde_json::Value =
                serde_json::from_str(log.lines().next().unwrap()).unwrap();
            assert_eq!(record["would_send"]["shouldQuery"], false);
        }
    }
