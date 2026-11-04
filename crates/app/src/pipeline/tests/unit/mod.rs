    use super::*;
    fn health_cues(a: &Pipeline) -> Vec<ui::HealthCue> {
        a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Health(c) => Some(c.clone()), _ => None }).collect()
    }
    fn cues(a: &Pipeline) -> Vec<ui::CommandCue> {
        a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Command(c) => Some(c.clone()), _ => None }).collect()
    }
    fn app() -> (tempfile::TempDir, Pipeline) {
        let t = tempfile::tempdir().unwrap();
        let mut a = Pipeline::new(
            t.path().join("data"),
            t.path().join("missing-models"),
            Settings::default(),
            None,
            true,
        )
        .unwrap();
        // The default model's default precision is on this (fixture) Mac, so picking it runs it rather than
        // downloading (pipeline/engine_models.rs); engine_models.rs covers the rest.
        let config = crate::inference_engine::Config::bundled();
        let source = config.model(&config.default_model).unwrap().source.clone();
        crate::inference_engine::models::fixture::install_quant(&a.engine_models_mut().roots.clone(), &source, "incoai/Qwen3.8-27B-DFlash2");
        a.engine_refresh();
        (t, a)
    }
    fn word(text: &str) -> Word {
        Word {
            text: text.into(),
            start_ns: 1_000_000_000,
            end_ns: 1_200_000_000,
            confidence: 1.,
        }
    }
    fn timed(text: &str, start: u64) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word { text: w.into(), start_ns: start + i as u64 * 300_000_000, end_ns: start + i as u64 * 300_000_000 + 250_000_000, confidence: 1. })
            .collect()
    }
    fn say(a: &mut Pipeline, source: &str, text: &str, start: u64, forced: bool) {
        let words = timed(text, start);
        let end = words.last().unwrap().end_ns;
        a.listen(ListenEvent::SpeechStart { source: source.into(), host_time_ns: start }).unwrap();
        a.listen(ListenEvent::PartialTranscript { source: source.into(), text: text.into(), words: words[..words.len().min(3)].to_vec(), stable_words: 0 }).unwrap();
        a.listen(ListenEvent::FinalUtterance { source: source.into(), text: text.into(), words, start_ns: start, end_ns: end, end_of_turn_probability: if forced { 0.1 } else { 0.9 }, forced }).unwrap();
    }
    fn note_rig() -> (tempfile::TempDir, Pipeline, String) {
        let (t,mut a) = app();
        let mic = crate::fake_source("mic",0,1);
        let source = mic.id.0.clone();
        a.source(mic);
        (t,a,source)
    }
    struct Silent;
    impl listen::SpeechDetector for Silent {
        fn probability(&mut self, _: &[f32; 512], _: &mut listen::onnx::VadState) -> Result<f32> {
            Ok(0.)
        }
    }
    impl listen::TurnDetector for Silent {
        fn probability(&mut self, _: &[f32]) -> Result<f32> {
            Ok(0.)
        }
    }
    impl listen::Recognizer for Silent {
        fn transcribe(&mut self, _: &[f32], _: u64) -> Result<listen::Transcript> {
            Ok(listen::Transcript::from_words(vec![]))
        }
    }

    /// One spoken command inside a long turn: the second at which it begins, its words, whether a partial
    /// transcript carried it before the window decode, and the decider's injected answer (None: the real local decider).
    struct Spoken { at_s: u64, phrase: &'static str, partial: bool, answer: Option<(decide::CommandIntent, decide::CommandSelection)> }
    fn look(at_s: u64, partial: bool, approve: bool) -> Spoken {
        let answer = if approve { (decide::CommandIntent::Yes, decide::CommandSelection::LookAtThis) } else { (decide::CommandIntent::FalseAlarm, decide::CommandSelection::Unsure) };
        Spoken { at_s, phrase: "Look at this", partial, answer: Some(answer) }
    }
    const S: u64 = 1_000_000_000;
    /// Faster-than-real-time replay of a long turn on the primary mic. Displays 1 and 2 show
    /// second `t` from 1 + t s (pixels encode t; a full-resolution grab encodes t + 100 in its
    /// third byte); the 30-s command audio ring is fed as capture does; a partial transcript of the words since
    /// the last segment arrives at the first step after the phrase's last word; the listen engine's 15-s window rolls every 13 s and emits
    /// words older than its 2-s overlap as segments; Smart Turn's final comes at the end.
    /// Returns (screen_command actions, chain and command_action errors).
    fn long_turn(a: &mut Pipeline, source: &str, seconds: u64, spoken: &[Spoken]) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let mut words: Vec<Word> = vec![];
        for i in 0..seconds * 2 {
            let t = S + i * S / 2;
            if spoken.iter().all(|c| t + S / 2 <= S + c.at_s * S || t >= S + c.at_s * S + S * c.phrase.split_whitespace().count() as u64 * 3 / 10 + S / 2) {
                words.push(Word { text: "words".into(), start_ns: t, end_ns: t + 250_000_000, confidence: 1. });
            }
        }
        // Each command opens its own sentence, as Parakeet punctuates it ("... words. Look at this").
        for c in spoken {
            if let Some(before) = words.iter_mut().rev().find(|w| w.start_ns < S + c.at_s * S) {
                before.text.push('.');
            }
        }
        for c in spoken { words.extend(timed(c.phrase, S + c.at_s * S)); }
        words.sort_by_key(|w| w.start_ns);
        a.command_test_answers.extend(spoken.iter().filter_map(|c| c.answer));
        let screen_now = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shown = screen_now.clone();
        a.screen_holds.set_grabber(Some(Arc::new(move |id| {
            let t = shown.load(Ordering::SeqCst);
            screen::Frame::new(screen::DisplayInfo { id, x: 0., y: 0., width: 4, height: 1 }, S + t * S, 4, 1, [(t >> 8) as u8, t as u8, 100 + id as u8].repeat(4))
        })));
        a.listen(ListenEvent::SpeechStart { source: source.into(), host_time_ns: S }).unwrap();
        let mut emitted = 0;
        let mut partial_sent = vec![false; spoken.len()];
        for t in 0..=seconds + 1 {
            let now = S + t * S;
            screen_now.store(t, Ordering::SeqCst);
            for id in [1u32, 2] {
                let rgb = [(t >> 8) as u8, t as u8, id as u8].repeat(2);
                a.screen_buffer.lock().unwrap().push(screen::Frame::new(screen::DisplayInfo { id, x: 0., y: 0., width: 2, height: 1 }, now, 2, 1, rgb).unwrap()).unwrap();
            }
            // As the capture path does: the 30-s ring, then held command audio.
            a.audio.entry(source.into()).or_insert(AudioRing { start: now, samples: VecDeque::new() }).push(now, &[0.01; 16000]);
            for (i, c) in spoken.iter().enumerate() {
                let said = timed(c.phrase, S + c.at_s * S).last().unwrap().end_ns;
                if c.partial && !partial_sent[i] && said <= now {
                    partial_sent[i] = true;
                    let heard: Vec<_> = words[emitted..].iter().filter(|w| w.end_ns <= now).cloned().collect();
                    a.listen(ListenEvent::PartialTranscript { source: source.into(), text: listen::Transcript::from_words(heard.clone()).text, words: heard, stable_words: 0 }).unwrap();
                }
            }
            if t > 0 && t % 13 == 0 && t < seconds {
                let n = words.partition_point(|w| w.end_ns + 640_000_000 < now - 2 * S);
                if n > emitted {
                    let seg = words[emitted..n].to_vec();
                    emitted = n;
                    a.listen(ListenEvent::TranscriptSegment { source: source.into(), text: listen::Transcript::from_words(seg.clone()).text, start_ns: seg[0].start_ns, end_ns: seg.last().unwrap().end_ns, words: seg }).unwrap();
                }
            }
            // Native grabs run on their own thread (~80 ms); the replay steps a whole second.
            a.screen_holds.settle();
        }
        let tail = words[emitted..].to_vec();
        a.listen(ListenEvent::FinalUtterance { source: source.into(), text: listen::Transcript::from_words(tail.clone()).text, start_ns: tail[0].start_ns, end_ns: tail.last().unwrap().end_ns, words: tail, forced: false, end_of_turn_probability: 0.9 }).unwrap();
        let records = a.index.evidence.records().unwrap();
        let actions = records.iter().filter(|r| r.kind == "screen_command").map(|r| r.payload["action"].clone()).collect();
        let errors = records.iter().filter(|r| r.kind == "command_action" || r.payload["phase"] == "chain").map(|r| r.payload["error"].clone()).collect();
        (actions, errors)
    }
    fn long_turn_app(_history_s: u64) -> (tempfile::TempDir, Pipeline, String) {
        let (t, mut a) = app();
        let i = crate::fake_source("mic", 0, 1);
        let source = i.id.0.clone();
        a.source(i);
        a.screen_buffer.lock().unwrap().reconfigure(screen::ScreenConfig { fps: 1, ..Default::default() }, 0).unwrap();
        (t, a, source)
    }
    /// Every archived capture retains phrase-time pixels. Separately assert that
    /// only the preferred resolution per display reaches the agent.
    fn archived_with_preferred_attachments(a: &Pipeline, action: &serde_json::Value) -> Vec<(String, u64, u64, u64, u8)> {
        let shots = action["shots"].as_array().unwrap_or_else(|| panic!("no attachment: {action}"));
        let records = a.index.evidence.records().unwrap();
        let mut preferred = vec![];
        let decoded = shots.iter().map(|shot| {
            let record = records.iter().find(|r|r.kind == "screenshot"
                && r.payload["display_id"] == shot["display_id"]
                && r.payload["captured_host_ns"] == shot["frame_ns"]
                && r.payload["kind"] == shot["kind"]).unwrap();
            let (_, bytes) = a.index.evidence.image_bytes(record.payload["image_id"].as_str().unwrap()).unwrap();
            if shot["kind"] == "full_resolution" || !shots.iter().any(|s|s["display_id"] == shot["display_id"] && s["kind"] == "full_resolution") {
                if !preferred.contains(&bytes) { preferred.push(bytes.clone()); }
            }
            let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            reader.next_frame(&mut pixels).unwrap();
            (shot["kind"].as_str().unwrap().into(), shot["display_id"].as_u64().unwrap(), (shot["frame_ns"].as_u64().unwrap() - S) / S, (pixels[0] as u64) << 8 | pixels[1] as u64, pixels[2])
        }).collect();
        assert_eq!(a.attachment_batch.iter().map(|i|i.bytes.clone()).collect::<Vec<_>>(), preferred);
        decoded
    }

    mod bug_notes;
    mod long_turns;
    mod menu_settings;
    mod engine_models; // Agent ▸ Model ▸ the engine's precisions, 28 Oct
    mod settings_tool; // the agent's settings tool, 26 Oct
    mod commands;
    mod calls_and_agent;
    mod sources;
    mod speakers_ignore; // island worker n1022: Don't ask again
    mod transcript_naming; // [chrome] n1022 stage 3
    mod voice_names; // quiet speaker naming, 23 Oct
