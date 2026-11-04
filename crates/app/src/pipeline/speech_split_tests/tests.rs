    use super::*;
    fn word(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.into(),
            start_ns: start,
            end_ns: end,
            confidence: 1.,
        }
    }
    fn seg(speaker: i32, start_time: f64, end_time: f64) -> speakers::diar::SpeakerSegment {
        speakers::diar::SpeakerSegment {
            speaker,
            start_time,
            end_time,
        }
    }
    const S: u64 = 1_000_000_000;
    fn timed(tokens: &[&str]) -> Vec<Word> {
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| word(t, i as u64 * S, (i + 1) as u64 * S))
            .collect()
    }
    fn tracks(runs: &[Run]) -> Vec<Option<i32>> {
        runs.iter()
            .map(|r| r.track.as_ref().map(|t| t.speaker))
            .collect()
    }
    struct Empty;
    impl listen::Recognizer for Empty {
        fn transcribe(&mut self, _: &[f32], _: u64) -> Result<listen::Transcript> {
            Ok(listen::Transcript::from_words(vec![]))
        }
    }
    impl listen::SpeechDetector for Empty {
        fn probability(&mut self, _: &[f32; 512], _: &mut listen::onnx::VadState) -> Result<f32> {
            Ok(0.)
        }
    }
    impl listen::TurnDetector for Empty {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(0.)
        }
    }
    fn empty_engine() -> Result<ListenEngine<Empty, Empty, Empty>> {
        ListenEngine::new(Empty, Empty, Empty, Default::default())
    }
    #[test]
    fn hung_worker_requeues_corrections_and_drains_retained_jobs() {
        let temp = tempfile::tempdir().unwrap();
        let mut p = Pipeline::new(
            temp.path().join("data"),
            temp.path().join("models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        let (runtime, pop) = crate::speech_split::Runtime::held_queue();
        assert!(pop());
        p.speech_split.runtime = Some(runtime);
        let (release, blocked) = std::sync::mpsc::channel();
        let old = Arc::new(
            listen::ListenWorker::spawn(Default::default(), move || {
                blocked
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                empty_engine()
            })
            .unwrap(),
        );
        p.live_worker = Some(old.clone());
        for key in ["first", "second"] {
            p.speech_split.reservations.insert(key.into(), JOB_BYTES);
            p.speech_split.reserved_bytes += JOB_BYTES;
            p.speech_split.decoded.insert(
                key.into(),
                Decoding {
                    input: Input {
                        region: heimdall_night::dataset::Region {
                            session: "s".into(),
                            source: "call".into(),
                            start_ns: 0,
                            end_ns: S,
                            speakers: 2,
                            utterance_ids: vec![],
                            audio: vec![],
                        },
                        start_ns: 0,
                        samples: vec![],
                        enrollments: EnrollmentStore::uncalibrated(),
                    },
                    output: Output {
                        tracks: (0..2)
                            .map(|_| heimdall_night::correction::Track {
                                samples: vec![0.; 3200],
                                identity: serde_json::Value::Null,
                            })
                            .collect(),
                        cpu_seconds: 0.,
                        separation_seconds: 0.,
                        total_seconds: 0.,
                    },
                    // Already finished slots survive replacement.
                    words: [Some(vec![word("kept", 0, S)]), None],
                    admitted: Instant::now(),
                },
            );
        }
        p.correction_tick().unwrap();
        assert_eq!(p.speech_split.inflight.len(), 2);
        let old_ids: Vec<_> = p.speech_split.inflight.keys().copied().collect();
        let (tx, _rx) = crossbeam_channel::bounded(512);
        p.abandon_worker("injected hung correction", &tx);
        release.send(()).unwrap();
        if let Some(replacement) = p.live_worker.take() {
            replacement.request_shutdown();
        }
        assert!(
            p.speech_split.inflight.is_empty(),
            "abandoned worker must release correction ownership"
        );
        let replacement =
            Arc::new(listen::ListenWorker::spawn(Default::default(), empty_engine).unwrap());
        replacement.wait_ready().unwrap();
        p.live_worker = Some(replacement.clone());
        p.correction_tick().unwrap();
        assert_eq!(p.speech_split.inflight.len(), 2);
        // An old forwarder's end marker is not a new worker failure.
        p.worker_ended(None).unwrap();
        assert_eq!(
            p.speech_split.inflight.len(),
            2,
            "stale end marker must preserve the new owner's requests"
        );
        for id in old_ids {
            assert!(!p
                .correction_transcript(id, Ok(listen::Transcript::from_words(vec![])), 0., 0.)
                .unwrap());
        }
        while !p.speech_split.decoded.is_empty() {
            if let Some(event @ listen::WorkerEvent::SideTranscript { .. }) =
                replacement.recv_event()
            {
                p.worker_event(event).unwrap();
            }
        }
        assert!(p.speech_split.inflight.is_empty());
        assert!(p.speech_split.reservations.is_empty());
        assert_eq!(p.speech_split.reserved_bytes, 0);
        assert!(pop(), "drained corrections request helper unload");
        let records = p.index.evidence.records().unwrap();
        let completed: Vec<_> = records
            .iter()
            .filter(|r| r.kind == "speech_split_completed")
            .collect();
        assert_eq!(completed.len(), 2);
        assert!(completed
            .iter()
            .all(|r| r.payload["tracks"][0]["owned_text"] == "kept"));
    }
    #[test]
    fn admission_matches_archive_after_diarization_compaction() {
        let temp = tempfile::tempdir().unwrap();
        let mut p = Pipeline::new(
            temp.path().join("data"),
            temp.path().join("models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        let source = "call";
        // Create the actual call owner without supplying audio to its model helper.
        let actor = crate::speakers_runtime::Actor::start(
            source.into(),
            0,
            p.models.clone(),
            p.session.clone(),
            NamingFlow::new(EnrollmentStore::uncalibrated()),
            p.speaker_cpu.clone(),
            p.speaker_tx.clone(),
            p.settings.speakers_backend.clone(),
        )
        .unwrap();
        p.discord.insert(
            source.into(),
            Discord {
                origin: 0,
                session: 0,
                actor_generation: 0,
                actor,
                speaking: false,
                fed_until: 0,
                active: false,
                loading: false,
                closed: false,
                segments: vec![],
                unlabelled: vec![],
            },
        );
        let words = timed(&[
            "first",
            "full",
            "long",
            "sentence.",
            "second",
            "full",
            "long",
            "sentence.",
        ]);
        let id = 1;
        let observation = agent::protocol::Observation {
            id: format!("{}:{id}", p.session.file_name().unwrap().to_string_lossy()),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: "Unknown".into(),
            language: "en".into(),
            text: listen::Transcript::from_words(words.clone()).text,
        };
        p.index
            .record(
                &observation,
                source,
                0,
                8 * S,
                &words,
                &[],
                &serde_json::json!({"status":"pending_decision"}),
            )
            .unwrap();
        p.call_final(id, source, 0, 8 * S, &words, &[]);
        p.speaker_names.insert(
            TrackId {
                source: source.into(),
                session: 0,
                speaker: 1,
            },
            "Ada".into(),
        );
        p.speaker_names.insert(
            TrackId {
                source: source.into(),
                session: 0,
                speaker: 2,
            },
            "Bob".into(),
        );
        // Stable finalized drains precede the compacted mutable snapshot, as in production.
        p.stable_speech(source, 0, 4 * S, vec![seg(1, 0., 4.)], true)
            .unwrap();
        p.speaker_ready(crate::speakers_runtime::Event {
            source: source.into(),
            generation: 0,
            update: crate::speakers_runtime::Update::Snapshot {
                origin: 0,
                segments: vec![seg(2, 4., 8.)],
                events: vec![],
            },
        })
        .unwrap();
        p.project_call(source).unwrap();
        let rows = p.index.utterances_between(0, 8 * S).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["speaker"], "Ada");
        assert_eq!(rows[1]["speaker"], "Bob");
        p.hooks.send_choices.push_back(decide::send::Choice::Wait);
        p.admit_agent_final(AgentFinal {
            audio_error: None,
            observation: observation.clone(),
            source: source.into(),
            start: 0,
            end: 8 * S,
            words: words.clone(),
            notes: vec![],
            images: vec![],
            turn: decide::Turn {
                id: observation.id,
                role: decide::Role::Other("Unknown".into()),
                transcript: observation.text,
                audio: decide::AudioClip::new(vec![0.; 3200]).unwrap(),
            },
            audio_spans: vec![],
            forced: false,
            end_of_turn_probability: 1.,
        })
        .unwrap();
        let expected = format!(
            "Ada: {}\nBob: {}",
            rows[0]["text"].as_str().unwrap(),
            rows[1]["text"].as_str().unwrap()
        );
        let records = p.index.evidence.records().unwrap();
        let routed = records
            .iter()
            .find(|r| r.kind == "agent_context_routing" && r.payload["given_to_agent"] == true)
            .unwrap();
        assert_eq!(routed.payload["observation"]["text"], expected);
        let snapshot = p.tools.send_context.lock().unwrap().snapshot();
        assert_eq!(snapshot.turns.last().unwrap().text, expected);
    }
    /// 19 Oct: "Speaker activation failed / Pipeline error: database is locked" during a call.
    /// A speaker revision reads before it writes; while another archive writer (the evidence
    /// writer, backfill) held the lock it failed at once and the final was dropped. It must wait.
    #[test]
    fn speaker_revision_waits_for_another_archive_writer_instead_of_failing() {
        let temp = tempfile::tempdir().unwrap();
        let p = Pipeline::new(temp.path().join("data"), temp.path().join("models"), Settings::default(), None, true).unwrap();
        let words = timed(&["one", "two", "three", "four"]);
        let o = agent::protocol::Observation {
            id: "final_lock:1".into(),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: "Unknown".into(),
            language: "en".into(),
            text: listen::Transcript::from_words(words.clone()).text,
        };
        p.index.record(&o, "call", 0, 4 * S, &words, &[], &serde_json::json!({"status":"pending_decision"})).unwrap();
        let root = p.index.evidence.root().to_owned();
        let (locked, wait_locked) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let other = heimdall_archive::store::open(&root).unwrap();
            other.execute_batch("BEGIN IMMEDIATE").unwrap();
            other.execute("INSERT INTO sessions(name,started_wall_ns) VALUES('session-lock-holder',0)", []).unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            other.execute_batch("COMMIT").unwrap();
        });
        wait_locked.recv().unwrap();
        let runs = word_runs(&words, "call", 0, &[seg(1, 0., 2.), seg(2, 2., 4.)], &BTreeMap::new());
        let started = std::time::Instant::now();
        assert!(p.index.revise_transcript(&o.id, "call", 1, &words, &runs, false).unwrap());
        assert!(started.elapsed() >= std::time::Duration::from_millis(200), "the revision waited for the lock holder");
        holder.join().unwrap();
        let rows = p.index.utterances_between(0, 4 * S).unwrap();
        assert!(!rows.is_empty() && rows.iter().all(|r| r["decision"]["transcript_revision"] == 1), "{rows:?}");
    }
    #[test]
    fn late_disposition_updates_all_split_rows_without_changing_text() {
        let temp = tempfile::tempdir().unwrap();
        let p = Pipeline::new(
            temp.path().join("data"),
            temp.path().join("models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        let words = timed(&[
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ]);
        let o = agent::protocol::Observation {
            id: "final_%:1".into(),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: "Unknown".into(),
            language: "en".into(),
            text: listen::Transcript::from_words(words.clone()).text,
        };
        for id in [&o.id, &format!("{}:speechless", o.id)] {
            let mut other = o.clone();
            other.id = id.clone();
            p.index
                .record(
                    &other,
                    "call",
                    0,
                    8 * S,
                    &words,
                    &[],
                    &serde_json::json!({"status":"pending_decision"}),
                )
                .unwrap();
        }
        let runs = word_runs(
            &words,
            "call",
            0,
            &[seg(1, 0., 4.), seg(2, 4., 8.)],
            &BTreeMap::new(),
        );
        p.index
            .revise_transcript(&o.id, "call", 1, &words, &runs, false)
            .unwrap();
        let before = p.index.utterances_between(0, 8 * S).unwrap();
        p.index
            .record(
                &o,
                "call",
                0,
                8 * S,
                &[],
                &[],
                &serde_json::json!({"status":"late_decision"}),
            )
            .unwrap();
        let rows = p.index.utterances_between(0, 8 * S).unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            if row["id"] == o.id || row["id"] == format!("{}:speech:1", o.id) {
                assert_eq!(row["decision"]["status"], "late_decision");
                assert_eq!(row["decision"]["transcript_revision"], 1);
            } else {
                assert_eq!(row["decision"]["status"], "pending_decision");
            }
        }
        let after = p.index.utterances_between(0, 8 * S).unwrap();
        assert_eq!(
            before.iter().map(|r| &r["text"]).collect::<Vec<_>>(),
            after.iter().map(|r| &r["text"]).collect::<Vec<_>>()
        );
        assert!(p
            .index
            .languages_for_test()
            .iter()
            .all(|language| language == "en"));
    }
    #[test]
    fn attribution_splits_substantial_changes_and_keeps_late_names() {
        let segments = vec![seg(1, 0., 4.), seg(2, 4., 8.)];
        let words = timed(&[
            "first",
            "full",
            "long",
            "sentence.",
            "second",
            "full",
            "long",
            "sentence.",
        ]);
        let mut names = BTreeMap::new();
        let runs = word_runs(&words, "call", 0, &segments, &names);
        assert_eq!(tracks(&runs), [Some(1), Some(2)]);
        names.insert(runs[1].track.clone().unwrap(), "Ada".into());
        let renamed = word_runs(&words, "call", 0, &segments, &names);
        assert_eq!(renamed[1].speaker, "Ada");
        assert_eq!(
            renamed[0].speaker, "Unknown",
            "unmatched substantial track is allowed"
        );
        // A coded voice's label is kept like a name (review of 23 Oct: it was archived as "Unknown"); anonymous
        // track names are not.
        names.insert(runs[0].track.clone().unwrap(), "Speaker Fri 23 Oct 21h #3".into());
        assert_eq!(word_runs(&words, "call", 0, &segments, &names)[0].speaker, "Speaker Fri 23 Oct 21h #3");
        names.insert(runs[0].track.clone().unwrap(), "Speaker 1".into());
        assert_eq!(word_runs(&words, "call", 0, &segments, &names)[0].speaker, "Unknown");
        assert_eq!(
            owner(&[seg(1, 0., 0.5), seg(2, 0.5, 1.)], 0, &word("tie", 0, S)),
            None
        );
    }
    #[test]
    fn ambiguous_words_attach_by_gap_then_preceding_unless_sentence_ends() {
        let segments = vec![seg(1, 0., 5.), seg(2, 4., 9.)];
        let mut words = timed(&[
            "one",
            "two",
            "three",
            "four",
            "ambiguous",
            "five",
            "six",
            "seven",
            "eight",
        ]);
        let runs = word_runs(&words, "call", 0, &segments, &BTreeMap::new());
        assert_eq!(tracks(&runs), [Some(1), Some(2)]);
        assert_eq!(runs[0].text(), "one two three four ambiguous");
        words[3].text.push('.');
        let runs = word_runs(&words, "call", 0, &segments, &BTreeMap::new());
        assert_eq!(runs[1].text(), "ambiguous five six seven eight");
        words[3].text = "four".into();
        words[4].start_ns += 100_000_000;
        let runs = word_runs(&words, "call", 0, &segments, &BTreeMap::new());
        assert_eq!(
            runs[1].text(),
            "ambiguous five six seven eight",
            "nearest time boundary wins"
        );
        let words = timed(&["leading", "one", "two", "three", "four", "trailing"]);
        let runs = word_runs(&words, "call", 0, &[seg(1, 1., 5.)], &BTreeMap::new());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text(), "leading one two three four trailing");
    }
    #[test]
    fn short_changes_merge_and_ambiguity_does_not_inflate_support() {
        let words = timed(&[
            "one", "two", "three", "four", "tiny", "turn", "five", "six", "seven", "eight",
        ]);
        let runs = word_runs(
            &words,
            "call",
            0,
            &[seg(1, 0., 4.), seg(2, 4., 6.), seg(1, 6., 10.)],
            &BTreeMap::new(),
        );
        assert_eq!(tracks(&runs), [Some(1)]);
        assert_eq!(runs[0].words.len(), words.len());
        // Four words alone cannot qualify; only 0.6 seconds are exclusive.
        let mut fast = timed(&[
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ]);
        for (i, w) in fast[4..].iter_mut().enumerate() {
            w.start_ns = 4 * S + i as u64 * 150_000_000;
            w.end_ns = w.start_ns + 150_000_000;
        }
        let runs = word_runs(
            &fast,
            "call",
            0,
            &[seg(1, 0., 4.), seg(2, 4., 4.6)],
            &BTreeMap::new(),
        );
        assert_eq!(tracks(&runs), [Some(1)]);
        // An ambiguous prefix does not turn one owned word into a turn.
        let runs = word_runs(
            &words[..7],
            "call",
            0,
            &[seg(2, 2., 3.), seg(1, 3., 7.)],
            &BTreeMap::new(),
        );
        assert_eq!(tracks(&runs), [Some(1)]);
        // A three-word run is suppressed even with three seconds of support.
        let runs = word_runs(
            &words[..7],
            "call",
            0,
            &[seg(1, 0., 4.), seg(2, 4., 7.)],
            &BTreeMap::new(),
        );
        assert_eq!(tracks(&runs), [Some(1)]);
    }
    #[test]
    fn substantial_threshold_is_inclusive_and_short_run_prefers_nearest_anchor() {
        let mut words = timed(&[
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ]);
        for (i, w) in words[4..].iter_mut().enumerate() {
            w.start_ns = 4 * S + i as u64 * 200_000_000;
            w.end_ns = if i == 3 {
                5 * S
            } else {
                w.start_ns + 200_000_000
            };
        }
        let segments = [seg(1, 0., 4.), seg(2, 4., 5.)];
        assert_eq!(
            word_runs(&words, "call", 0, &segments, &BTreeMap::new()).len(),
            2
        );
        words[7].end_ns -= 1_000;
        assert_eq!(
            word_runs(&words, "call", 0, &segments, &BTreeMap::new()).len(),
            1
        );
        let mut words = timed(&[
            "one", "two", "three", "four", "tiny", "five", "six", "seven", "eight",
        ]);
        words[4].start_ns += 100_000_000;
        let runs = word_runs(
            &words,
            "call",
            0,
            &[seg(1, 0., 4.), seg(3, 4., 5.), seg(2, 5., 9.)],
            &BTreeMap::new(),
        );
        assert_eq!(tracks(&runs), [Some(1), Some(2)]);
        assert_eq!(runs[1].text(), "tiny five six seven eight");
    }
    #[test]
    fn exclusive_support_excludes_overlap_silence_and_duplicate_word_time() {
        let words = vec![word("a", 0, S), word("b", 0, S), word("c", 0, S)];
        let refs: Vec<_> = words.iter().collect();
        assert!(
            (exclusive_seconds(
                &refs,
                1,
                0,
                &[seg(1, 0., 1.), seg(1, 0., 1.), seg(2, 0.6, 1.)]
            ) - 0.6)
                .abs()
                < 1e-9
        );
        let words = vec![
            word("a", 0, 100_000_000),
            word("b", 10 * S, 10 * S + 100_000_000),
            word("c", 20 * S, 20 * S + 100_000_000),
        ];
        let refs: Vec<_> = words.iter().collect();
        assert!((exclusive_seconds(&refs, 1, 0, &[seg(1, 0., 21.)]) - 0.3).abs() < 1e-9);
    }
    #[test]
    fn absent_or_fragmented_evidence_keeps_one_line_without_losing_words() {
        let words = timed(&["one", "two", "three", "four"]);
        let absent = word_runs(&words, "call", 0, &[], &BTreeMap::new());
        assert_eq!(tracks(&absent), [None]);
        assert_eq!(absent[0].text(), "one two three four");
        let fragmented = word_runs(
            &words,
            "call",
            0,
            &[
                seg(2, 0., 1.),
                seg(1, 1., 2.),
                seg(2, 2., 3.),
                seg(1, 3., 4.),
            ],
            &BTreeMap::new(),
        );
        assert_eq!(
            tracks(&fragmented),
            [Some(2)],
            "exact support tie uses first occurrence"
        );
        let empty = word_runs(&[], "call", 0, &[], &BTreeMap::new());
        assert!(empty.is_empty());
    }
    #[test]
    fn rejected_admission_retries_after_expiry_and_pcm_has_count_and_byte_backpressure() {
        let temp = tempfile::tempdir().unwrap();
        let mut p = Pipeline::new(
            temp.path().join("data"),
            temp.path().join("models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        let (runtime, pop) = crate::speech_split::Runtime::held_queue();
        p.speech_split.runtime = Some(runtime);
        p.stable_speech(
            "call",
            0,
            4_000_000_000,
            vec![seg(1, 1., 2.), seg(2, 1., 2.)],
            false,
        )
        .unwrap();
        assert_eq!(p.speech_split.pending.len(), 1);
        assert!(p.speech_split.submitted.is_empty());
        assert_eq!(p.speech_split.outstanding, 0);
        p.correction_end("call", 0);
        assert!(pop());
        p.correction_tick().unwrap();
        assert!(p.speech_split.pending.is_empty());
        assert_eq!(p.speech_split.submitted.len(), 1);
        assert_eq!(p.speech_split.reserved_bytes, JOB_BYTES);
        assert!(pop());
        p.stable_speech(
            "call",
            0,
            8_000_000_000,
            vec![seg(1, 1., 6.), seg(2, 1., 2.), seg(2, 4., 5.)],
            false,
        )
        .unwrap();
        assert_eq!(p.speech_split.reservations.len(), 2);
        assert!(pop());
        p.stable_speech(
            "call",
            0,
            10_000_000_000,
            vec![
                seg(1, 1., 9.),
                seg(2, 1., 2.),
                seg(2, 4., 5.),
                seg(2, 7., 8.),
            ],
            false,
        )
        .unwrap();
        assert_eq!(p.speech_split.pending.len(), 1);
        assert!(
            !pop(),
            "CPU separation pauses while both retained slots await ASR"
        );
        let key = p.speech_split.reservations.keys().next().unwrap().clone();
        p.release_correction(&key);
        p.correction_tick().unwrap();
        assert!(p.speech_split.pending.is_empty());
        assert!(pop());
        assert!(p.speech_split.reserved_bytes <= MAX_RETAINED_BYTES);
        // Byte bound applies independently of count.
        p.speech_split.reservations.clear();
        p.speech_split.reserved_bytes = MAX_RETAINED_BYTES;
        p.speech_split.pending.insert(
            "byte-blocked".into(),
            Pending {
                source: "call".into(),
                origin: 0,
                start: 1,
                end: 2,
            },
        );
        p.correction_tick().unwrap();
        assert_eq!(p.speech_split.pending.len(), 1);
        assert!(!pop());
    }
    #[test]
    fn diagnostic_gate_is_per_owned_edit_and_identity_never_certifies_text() {
        let output = Output {
            tracks: (0..2)
                .map(|_| heimdall_night::correction::Track {
                    samples: vec![],
                    identity: serde_json::json!({"Known":{"name":"Ada","cosine":0.99}}),
                })
                .collect(),
            cpu_seconds: 0.,
            separation_seconds: 0.,
            total_seconds: 0.,
        };
        let mut bad = word("Snowpolitik", 1, 2);
        bad.confidence = 0.4;
        let words = [
            Some(vec![word("serious", 1, 2), word("right", 2, 3)]),
            Some(vec![bad]),
        ];
        let gate = evidence_tracks(&words, &output, 0, 4, &[]);
        assert_eq!(gate[0]["would_publish"], true);
        assert_eq!(gate[1]["would_publish"], false);
        assert_eq!(gate[0]["published"], false);
        let repeated = [
            Some(vec![word("the", 1, 2), word("the", 2, 3)]),
            Some(vec![]),
        ];
        assert_eq!(
            evidence_tracks(&repeated, &output, 0, 4, &[])[0]["would_publish"],
            false
        );
        let duplicates = [words[0].clone(), words[0].clone()];
        assert!(evidence_tracks(&duplicates, &output, 0, 4, &[])
            .iter()
            .all(|g| g["would_publish"] == false));
        assert_eq!(
            evidence_tracks(&words, &output, 3, 4, &[])[0]["would_publish"],
            false,
            "context cannot count toward owned phrase"
        );
    }
    #[test]
    fn overlap_admission_splits_pair_changes_and_skips_three_way() {
        let segments = vec![seg(1, 0., 4.), seg(2, 0., 2.), seg(3, 1., 4.)];
        let out = overlaps(&segments, 0, 5_000_000_000);
        assert_eq!(
            out,
            vec![
                (0, 1_000_000_000, vec![1, 2]),
                (2_000_000_000, 4_000_000_000, vec![1, 3])
            ]
        );
    }
    #[test]
    fn archive_and_context_revision_are_idempotent_and_non_actionable() {
        let temp = tempfile::tempdir().unwrap();
        let mut p = Pipeline::new(
            temp.path().join("data"),
            temp.path().join("models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        let source = "call";
        let id = 1;
        let observation_id = format!("{}:{id}", p.session.file_name().unwrap().to_string_lossy());
        let words = vec![
            word("one", 0, 250_000_000),
            word("two", 250_000_000, 500_000_000),
            word("three", 500_000_000, 750_000_000),
            word("four", 750_000_000, 1_000_000_000),
            word("bridge", 1_000_000_000, 1_200_000_000),
            word("five", 1_200_000_000, 1_450_000_000),
            word("six", 1_450_000_000, 1_700_000_000),
            word("seven", 1_700_000_000, 1_950_000_000),
            word("eight", 1_950_000_000, 2_200_000_000),
        ];
        let o = agent::protocol::Observation {
            id: observation_id.clone(),
            timestamp_ms: 1,
            source: "Discord".into(),
            speaker: "Unknown".into(),
            language: "en".into(),
            text: "one two three four bridge five six seven eight".into(),
        };
        p.index
            .record(
                &o,
                source,
                0,
                3_000_000_000,
                &words,
                &[],
                &serde_json::json!({"status":"already_sent"}),
            )
            .unwrap();
        let seq = p.tools.send_context.lock().unwrap().observe_at(
            "Unknown",
            "one two three four bridge five six seven eight",
            3_000_000_000,
            serde_json::to_value(&o).unwrap(),
        );
        p.tools.send_context.lock().unwrap().sent(seq);
        p.speech_split.finals.insert(
            id,
            Final {
                source: source.into(),
                origin: 0,
                start: 0,
                end: 3_000_000_000,
                words: words.clone(),
                notes: vec![],
                last: String::new(),
                projected: vec![],
                revision: 0,
            },
        );
        p.speech_split.timelines.insert(
            (source.into(), 0),
            Timeline {
                scanned: 0,
                segments: vec![seg(1, 0., 1.2), seg(2, 1., 3.)],
                finalized: vec![],
            },
        );
        p.project_call(source).unwrap();
        p.project_call(source).unwrap();
        let rows = p.index.utterances_between(0, 3_000_000_000).unwrap();
        assert_eq!(rows.len(), 2);
        let events = p.index.evidence.records().unwrap();
        let revisions: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "transcript_revision")
            .collect();
        assert_eq!(revisions.len(), 1);
        assert_eq!(
            revisions[0].payload["original_words"],
            serde_json::to_value(words).unwrap()
        );
        let updates = p.hooks.ui.lock().unwrap();
        let last = updates
            .iter()
            .rev()
            .find_map(|update| match update {
                ui::UiUpdate::TranscriptRuns { lines, .. } => Some(lines),
                _ => None,
            })
            .unwrap();
        assert_eq!(last.len(), 2);
        assert!(
            matches!(&last[0].segments[0], ui::Segment::Words(text) if text == "one two three four bridge")
        );
        assert!(
            matches!(&last[1].segments[0], ui::Segment::Words(text) if text == "five six seven eight")
        );
        drop(updates);
        let snapshot = p.tools.send_context.lock().unwrap().snapshot();
        assert!(snapshot.pending.is_empty());
        assert_eq!(snapshot.through, seq);
        assert_eq!(snapshot.turns.len(), 1);
        assert!(snapshot.turns[0].text.contains("Unknown: one"));
        assert_eq!(p.decisions, 0);
        assert_eq!(p.finals, 0);
        assert!(p.command_candidates.is_empty());
        let key = "recovered".to_owned();
        let input = Input {
            region: heimdall_night::dataset::Region {
                session: "s".into(),
                source: source.into(),
                start_ns: 700_000_000,
                end_ns: 1_300_000_000,
                speakers: 2,
                utterance_ids: vec![observation_id.clone()],
                audio: vec![],
            },
            start_ns: 0,
            samples: vec![],
            enrollments: EnrollmentStore::uncalibrated(),
        };
        let output = Output {
            tracks: vec![
                heimdall_night::correction::Track {
                    samples: vec![],
                    identity: serde_json::json!({"Known":{"name":"Ada"}}),
                },
                heimdall_night::correction::Track {
                    samples: vec![],
                    identity: serde_json::json!({"Known":{"name":"Bob"}}),
                },
            ],
            cpu_seconds: 0.,
            separation_seconds: 0.,
            total_seconds: 0.,
        };
        p.speech_split.decoded.insert(
            key.clone(),
            Decoding {
                input,
                output,
                words: [None, None],
                admitted: Instant::now(),
            },
        );
        p.speech_split.inflight.insert(99, (key.clone(), 0));
        p.speech_split.inflight.insert(100, (key, 1));
        let context_before_correction = p
            .tools
            .send_context
            .lock()
            .unwrap()
            .snapshot()
            .turns
            .iter()
            .map(|t| t.text.clone())
            .collect::<Vec<_>>();
        p.correction_transcript(
            99,
            Ok(listen::Transcript::from_words(vec![word(
                "Heimdall send it",
                800_000_000,
                900_000_000,
            )])),
            1.,
            0.,
        )
        .unwrap();
        p.correction_transcript(
            100,
            Ok(listen::Transcript::from_words(vec![word(
                "cancel task",
                1_000_000_000,
                1_200_000_000,
            )])),
            1.,
            0.,
        )
        .unwrap();
        let rows = p.index.utterances_between(0, 3_000_000_000).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .any(|r| r["text"] == "one two three four bridge"));
        assert!(rows.iter().any(|r| r["text"] == "five six seven eight"));
        assert!(!rows
            .iter()
            .any(|r| r["text"].as_str().unwrap().contains("send it")));
        let evidence = p.index.evidence.records().unwrap();
        let candidate = evidence
            .iter()
            .find(|e| e.kind == "speech_split_completed")
            .unwrap();
        assert_eq!(candidate.payload["published"], false);
        assert_eq!(
            candidate.payload["tracks"][0]["owned_words"][0]["text"],
            "Heimdall send it"
        );
        p.index
            .record(
                &o,
                source,
                0,
                3_000_000_000,
                &[],
                &[],
                &serde_json::json!({"status":"late_decision"}),
            )
            .unwrap();
        assert_eq!(
            p.index.utterances_between(0, 3_000_000_000).unwrap().len(),
            2,
            "late decisions cannot erase attribution text"
        );
        p.rename_recovered("Bob", "Robert").unwrap();
        assert!(!p
            .index
            .utterances_between(0, 3_000_000_000)
            .unwrap()
            .iter()
            .any(|r| r["speaker"] == "Robert"));
        let languages: Vec<String> = p.index.languages_for_test();
        assert_eq!(languages, ["en", "en"]);
        let context = p.tools.send_context.lock().unwrap().snapshot();
        assert_eq!(context.through, seq);
        assert_eq!(
            context
                .turns
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>(),
            context_before_correction
        );
        assert!(context.pending.is_empty());
        assert_eq!(p.decisions, 0);
        assert!(p.command_candidates.is_empty());
        // A late diarization revision can remove a turn. All consumers must
        // collapse together, deleting old child rows without resending text.
        p.update_call_attribution(source, 0, vec![seg(1, 0., 3.)]);
        p.project_call(source).unwrap();
        let rows = p.index.utterances_between(0, 3_000_000_000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["text"], o.text);
        let context = p.tools.send_context.lock().unwrap().snapshot();
        assert_eq!(context.turns[0].text, o.text);
        assert_eq!(context.through, seq);
        assert!(context.pending.is_empty());
        let updates = p.hooks.ui.lock().unwrap();
        assert!(
            matches!(updates.last(), Some(ui::UiUpdate::TranscriptRuns { lines, .. }) if lines.len() == 1)
        );
    }
