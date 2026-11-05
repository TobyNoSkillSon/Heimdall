//! Speech glue between the agent and `speak`: the lazily loaded synthesizer, the voice-speed sample, delivery-gated
//! synthesis and output, and the level, words and speaking taps the island reads.
use super::*;

/// Load the synthesizer on its own worker only after the first requested reply. Replies and the bank fill share
/// one loaded model.
pub(crate) struct LazySynth {
    pub(super) drama3: Option<Arc<speak::drama3::Service>>,
    pub(super) session: PathBuf,
    pub(super) models: PathBuf,
    pub(super) model: Arc<Mutex<Option<speak::Supertonic>>>,
    pub(super) backend: String,
    /// Keep each synthesized chunk in the voice bank store as training data (agent replies).
    pub(super) record: Option<(Arc<crate::voice_bank::Bank>, voicebank::Origin)>,
    /// Voice speed in percent (Speaking ▸ Speed), read at each utterance.
    pub(super) speed: Arc<AtomicU16>,
    /// The Speed slider's sample: Jarvis only, loading him first if he is not resident (`None`: agent speech).
    pub(super) sample: Option<SampleVoice>,
    /// Speaking ▸ the voice (night 1022 menu), read per utterance; `None` (bank fill, tests): Jarvis, then Supertonic.
    pub(super) voices: Option<Arc<crate::speech_voice::Voices>>,
}
/// The Speed slider's sample voice ("the speed test … doesn't have a Jarvis voice"): the sample waits for
/// Jarvis to load, the slider row says so meanwhile, and no other voice ever speaks it. `hold` keeps Jarvis resident
/// for the sample even while Heimdall is paused or Speaking is off (the residency tick reads it).
#[derive(Clone)]
pub(crate) struct SampleVoice {
    pub(crate) hold: Arc<Mutex<Option<std::time::Instant>>>,
    /// Bumped by each new sample, so a stale "Jarvis unavailable" never clears a newer row state.
    pub(crate) generation: Arc<AtomicU64>,
    /// The slider row's note (`UiUpdate::VoiceSample`).
    pub(crate) note: Arc<dyn Fn(Option<String>) + Send + Sync>,
}
impl SampleVoice {
    pub(super) fn show(&self, note: Option<&str>) {
        (self.note)(note.map(str::to_owned));
    }
    /// The sample is over: Jarvis stays resident a little longer for the next slider move, then the usual residency
    /// applies again.
    pub(super) fn release(&self) {
        *self.hold.lock().unwrap() = Some(std::time::Instant::now() + SAMPLE_KEEP);
    }
}
/// How long the sample waits for Jarvis to load: a warm load is 5.1 s (18 Oct); a cold disk reads 27 GB first.
pub(super) const SAMPLE_LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(90);
/// Jarvis stays resident this long after a sample while nothing else wants him (the next slider move speaks at once).
pub(super) const SAMPLE_KEEP: std::time::Duration = std::time::Duration::from_secs(45);
/// What the island shows of Heimdall's voice, straight from playback to the UI channel: each sentence as it starts
/// playing (`SpokenText`, `None` at the end), and the loudness for the waveform (~33 updates a second while a voice
/// plays, none otherwise). Display signals, not journalled.
pub(crate) fn attach_speech_level(ui: Option<ui::Ui>, tools: std::sync::Weak<Tools>) {
    *SPEECH_CUT_UI.write().unwrap() = ui.clone();
    speak::set_level_hook(ui.map(|u| Arc::new(move |level: f32| {
        u.send(ui::UiUpdate::SpeechLevel((level.clamp(0., 1.) * 1000.).round() as u16));
    }) as speak::LevelHook));
    // Echo guard (23 Oct): each sentence through speakers is the reference its echo on the microphone is compared
    // with (pipeline/echo_guard.rs); the route is read as it starts.
    let echo = tools.clone();
    let echo_for_calls = tools.clone();
    speak::set_spoken_text_hook(Some(Arc::new(move |text: Option<&str>| {
        with_tools(&echo, |t| {
            let now = capture::host_now_ns();
            // A sentence of the answer starting while the speaking flag is still up (an ack's linger): its first audio.
            if text.is_some() && !t.voice_bank.ack_playing() { crate::latency::audio(); }
            match text {
                Some(s) => {
                    // A line queued in a call that starts after it ends plays at full level, and the other way round.
                    t.apply_call_level();
                    t.speech_policy.sentence_started(s, now, crate::pipeline::output_route::current() == crate::pipeline::output_route::Route::Speakers)
                }
                None => t.speech_policy.voice_stopped(now),
            }
        });
        if let Some(u) = ui { u.send(ui::UiUpdate::SpokenText(text.map(str::to_owned))); }
    }) as speak::SpokenTextHook));
    // One flag for agent speech and the Speed sample: `speak` raises it when the first sample reaches the speaker
    // and drops it when the last one has passed.
    super::hold::HOOKED.store(true, Ordering::Release);
    // Also the latency map's first-audio mark (lab/latency), with or without a UI.
    speak::set_speaking_hook(Some(Arc::new(move |on: bool| {
        // Speech starting while Heimdall is audible is not the user's turn by itself (`hold`).
        super::hold::AUDIBLE.store(on, Ordering::Release);
        if !on { with_tools(&tools, |t| t.speech_policy.voice_stopped(capture::host_now_ns())); }
        // The latency map's first audible answer: an acknowledgement playing is logged apart.
        if on {
            let mut ack = false;
            with_tools(&tools, |t| ack = t.voice_bank.ack_playing());
            if ack { crate::latency::ack_audible() } else { crate::latency::audio() }
        }
        if let Some(u) = ui { u.send(ui::UiUpdate::HeimdallSpeaking(on)); }
    }) as speak::SpeakingHook));
    install_call_voice_hook(&echo_for_calls);
}
/// Bug note 2026-11-05T02-32-17: speak only in a quiet gap, and stop as soon as anyone talks, guests included. A voice
/// opening on a call source holds a reply waiting to start until the gap (`hold::OwnerVoice::other_voice`), and one still open `GUEST_CUT_AFTER` later while Heimdall is audible stops him
/// (`Tools::guest_talked_over`). Nothing runs while Heimdall is silent but the hold's bookkeeping.
pub(crate) fn install_call_voice_hook(tools: &std::sync::Weak<Tools>) {
    let Some(t) = tools.upgrade() else { return };
    let weak = tools.clone();
    t.speech_policy.set_call_voice_hook(Some(Arc::new(move |source: &str, at: Option<u64>| {
        let mut speaking = false;
        with_tools(&weak, |t| { t.owner.other_voice(source, at.is_some()); speaking = t.heimdall_speaking(); });
        let Some(at) = at.filter(|_| speaking) else { return };
        let (weak, source) = (weak.clone(), source.to_owned());
        std::thread::spawn(move || {
            std::thread::sleep(super::speech_policy::GUEST_CUT_AFTER);
            with_tools(&weak, |t| t.guest_talked_over(&source, at));
        });
    })));
}
/// Runs `f` on the tools if they are still alive. These hooks run inside speak's playback lock: if the hook held the
/// last handle, dropping Tools there could take that lock again, so the last handle is dropped on its own thread.
fn with_tools(tools: &std::sync::Weak<Tools>, f: impl FnOnce(&Tools)) {
    let Some(t) = tools.upgrade() else { return };
    f(&t);
    if Arc::strong_count(&t) == 1 { std::thread::spawn(move || drop(t)); }
}
/// The island's handle for cut audio (`cut_speech`), set with the speech hooks.
static SPEECH_CUT_UI: std::sync::RwLock<Option<ui::Ui>> = std::sync::RwLock::new(None);
/// Heimdall's audio is being cut by an interrupt: the island ends the speaking look at once. Call before the cut, so
/// the speaking flag's fall (which follows it) finds the look already over.
pub(super) fn cut_speech() {
    if let Some(u) = SPEECH_CUT_UI.read().unwrap().as_ref() {
        u.send(ui::UiUpdate::HeimdallSpeechCut);
    }
}
/// What the Speed slider plays when released.
pub const VOICE_SAMPLE: &str = "This is how fast I will speak from now on.";
/// How long a line waits for a loading Jarvis before the lighter voice speaks it. A warm load measured 5.1 s
/// (18 Oct, sidecar spawn to ready: weights 2.7 s, voice 0.9 s, warm-up 0.7 s); a cold disk adds the 27 GB read.
pub(super) const JARVIS_LOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long a Jarvis line waits for the residency tick to claim him (the tick runs every 2 s).
pub(super) const JARVIS_CLAIM_WAIT: std::time::Duration = std::time::Duration::from_millis(2500);
impl speak::Synthesizer for LazySynth {
    /// Both voices are stretched to the user's speed after synthesis: playback's pre-buffer sizes itself by it.
    fn pace(&self) -> f32 { self.rate() }
    fn synthesize(&mut self, t: &str, l: &str, c: &dyn Fn() -> bool) -> Result<speak::Audio> {
        if c() {
            anyhow::bail!("speech interrupted");
        }
        let plain=speak::drama3::normalize(t);let t=plain.as_str();
        anyhow::ensure!(!t.is_empty(),"no spoken text");
        if let Some(drama3) = self.drama3.as_ref().filter(|_| self.route() == crate::speech_voice::Route::Jarvis) {
            if drama3.ready() {
                match drama3.render(t, c) {
                    Ok(mut audio) => {
                        self.record_drama3(t, l, &audio);
                        audio.samples = speak::stretch::stretch(&audio.samples, self.rate());
                        return Ok(audio);
                    },
                    Err(_) if c() => anyhow::bail!("speech interrupted"),
                    // The lighter voice says it now; one `voice: fallback` line says why (the log sink adds the time).
                    Err(e) => {
                        let error = format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ");
                        eprintln!("voice: fallback cause={} from=jarvis to=lighter error=\"{}\" · Jarvis could not say \"{}\", the lighter voice said it",
                            speak::drama3::fallback_cause(&error), error.chars().take(240).collect::<String>(), t.chars().take(160).collect::<String>());
                    }
                }
            }
        }
        // Public edition: Phonon S is the lighter voice too (that bundle has no Supertonic).
        if crate::edition::PUBLIC {
            return self.phonon_lighter(t, c);
        }
        let mut inner = self.model.lock().unwrap();
        if inner.is_none() {
            let mut model = match self.backend.as_str() {
                "cpu" => speak::Supertonic::load_with_backend(&self.models, speak::TtsBackend::Cpu)?,
                "metal" => speak::Supertonic::load_with_backend(&self.models, speak::TtsBackend::Metal)?,
                _ => speak::Supertonic::load(&self.models)?,
            };
            let evidence = speech_evidence(&self.session);
            let mut invocation = 0_u64;
            model.set_evidence_hook(Box::new(move |mut payload| {
                invocation += 1;
                payload["invocation"] = json!(invocation);
                evidence.send("tts_model_output", payload);
                Ok(())
            }));
            *inner = Some(model);
        }
        let model = inner.as_mut().unwrap();
        let result=speak::Synthesizer::synthesize(model, t, l, c);
        if let Err(e)=&result {
            // Noted, never in the way: the voice's own error is what the line reports.
            speech_evidence(&self.session).send("tts_error", json!({"text":t,"language":l,"error":format!("{e:#}")}));
        }
        if let (Ok(audio), Some((bank, origin))) = (&result, &self.record) {
            bank.record(voicebank::ClipMeta { origin: *origin, line_id: None, text: t.into(), language: l.into(), voice: model.voice().into(),
                generator: crate::voice_bank::GENERATOR.into(), settings: voicebank::supertonic_settings(&self.backend) }, audio.clone());
        }
        // Kept at natural speed above; heard at the user's speed, stretched like Jarvis (pitch kept).
        result.map(|mut audio| { audio.samples = speak::stretch::stretch(&audio.samples, self.rate()); audio })
    }
    fn stream_packets(&mut self, t:&str, l:&str, c:&dyn Fn()->bool, emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>) -> Result<speak::Audio> {
        // `written` keeps the punctuation for the island's text; each voice says the plain form.
        let written=speak::drama3::strip_tags(t);
        let plain=speak::drama3::normalize(t);let t=plain.as_str();
        anyhow::ensure!(!t.is_empty(),"no spoken text");
        // Speaking ▸ the voice (night 1022 menu): Supertonic alone, or a streaming voice; Jarvis below as before.
        let route=self.route();
        match &route {
            crate::speech_voice::Route::Jarvis => {},
            crate::speech_voice::Route::Supertonic => return self.lighter_line(&written,l,c,emit),
            streamed => { let streamed=streamed.clone(); return self.streamed_line(&streamed,&written,l,c,emit); },
        }
        if let Some(sample)=self.sample.clone() {
            let mine=sample.generation.load(Ordering::Acquire);
            let result=self.sample_packets(&sample,&written,t,l,c,emit);
            // A newer sample (it replaced this one) keeps its own, longer hold while Jarvis may still be loading.
            if sample.generation.load(Ordering::Acquire)==mine { sample.release(); }
            return result;
        }
        if let Some(drama3) = self.drama3.clone() {
            // Right after Heimdall or Speaking turns on, or after an interruption he was slow to leave, Jarvis may
            // be reloading: wait for him rather than switch voices (bounded; a failed load returns at once).
            // Only the residency tick claims him (a speech thread never overrides Speaking off, review 24 Oct). A line
            // that comes before its first tick (it runs every 2 s, Listening on or off) waits for that claim briefly
            // rather than go to the lighter voice unannounced (bug note 2026-10-30T18-42-08).
            if !drama3.wanted() && self.voices.is_some() {
                let until = std::time::Instant::now() + JARVIS_CLAIM_WAIT;
                while !drama3.wanted() && !c() && std::time::Instant::now() < until {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            drama3.resume();
            if !drama3.ready() && drama3.loading() {
                drama3.wait_ready(JARVIS_LOAD_WAIT, c);
            }
            // Jarvis says the line; a sentence he cannot say goes to the lighter voice alone and he goes on, and every
            // switch reaches the island (`speak_line`). The lighter voice never tries Jarvis again through synthesize().
            let slot=self.drama3.take();
            // Held out for memory (bug note 2026-11-03T22-38-44): Phonon S says the lines, not Supertonic; asked per
            // sentence, so a give-way mid-line moves the rest of it to Phonon S too.
            let result=speak::drama3::speak_line(&drama3,&mut LineVoices{synth:self,language:l,service:&drama3},&written,c,emit);
            self.drama3=slot;
            return result;
        }
        // No Jarvis in this build (tests, bank fill).
        self.lighter_line(&written,l,c,emit)
    }
}
impl LazySynth {
    /// The public edition's lighter voice: the chosen Phonon S preset says the whole text, stretched to the user's speed.
    fn phonon_lighter(&self, t: &str, c: &dyn Fn() -> bool) -> Result<speak::Audio> {
        let voices = self.voices.clone().context("no voice choice")?;
        let preset = match voices.route() { crate::speech_voice::Route::Phonon(p) => p, _ => voices.choice().phonon_preset };
        voices.speak_sentence(&crate::speech_voice::Route::Phonon(preset), t, t, self.rate(), c, &mut |_| Ok(()))
    }
    /// Which voice says the next utterance (Jarvis without a voice choice: bank fill, tests).
    pub(super) fn route(&self) -> crate::speech_voice::Route {
        self.voices.as_ref().map_or(crate::speech_voice::Route::Jarvis, |v| v.route())
    }
    /// A line through Phonon S or ElevenLabs, sentence by sentence at the user's speed. A sentence the voice cannot say
    /// (model missing, network down, key refused) is said by the lighter voice instead, with one log line; one it
    /// stops partway through is left there and the line goes on.
    pub(super) fn streamed_line(&mut self, route:&crate::speech_voice::Route, written:&str, l:&str, c:&dyn Fn()->bool,
        emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>) -> Result<speak::Audio> {
        let voices=self.voices.clone().context("no voice choice")?;
        let label=match route { crate::speech_voice::Route::ElevenLabs(_)=>"ElevenLabs", crate::speech_voice::Route::Own(_)=>"Own Engine", _=>"Phonon S" };
        // The lighter voice never tries Jarvis meanwhile.
        let drama3=self.drama3.take();
        let result=(|| -> Result<speak::Audio> {
            let mut all=speak::Audio{samples:vec![],sample_rate:speak::voice::PLAYBACK_RATE,first_audio_ms:0.,synthesis_ms:0.};
            for sentence in speak::drama3::display_sentences(written) {
                anyhow::ensure!(!c(),"speech interrupted");
                let plain=speak::drama3::normalize(&sentence);
                if plain.is_empty() {continue;}
                let mut heard=false;
                let said=voices.speak_sentence(route,&sentence,&plain,self.rate(),c,&mut |packet| {
                    if matches!(packet,speak::SpeechPacket::Audio(_)) {heard=true;}
                    emit(packet)
                });
                let audio=match said {
                    Ok(audio)=>audio,
                    Err(_) if c()=>anyhow::bail!("speech interrupted"),
                    Err(e) if heard=>{eprintln!("voice: {label} stopped partway through \"{}\": {e:#}",sentence.chars().take(160).collect::<String>());continue;},
                    Err(e)=>{
                        eprintln!("voice: {label} could not say \"{}\", the lighter voice said it: {}",sentence.chars().take(160).collect::<String>(),
                            format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" "));
                        let audio=speak::Synthesizer::synthesize(self,&sentence,l,c)?;
                        if c() {break;}
                        emit(speak::SpeechPacket::Sentence(sentence))?;
                        emit(speak::SpeechPacket::Audio(audio.clone()))?;
                        audio
                    },
                };
                if all.samples.is_empty() {all.first_audio_ms=audio.first_audio_ms;}
                all.samples.extend_from_slice(&audio.samples);all.synthesis_ms+=audio.synthesis_ms;
            }
            Ok(all)
        })();
        self.drama3=drama3;
        result
    }
    /// The lighter voice (Supertonic) says the line: one sentence at a time, so each reaches the island as it starts
    /// playing.
    pub(super) fn lighter_line(&mut self, written:&str, l:&str, c:&dyn Fn()->bool,
        emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>) -> Result<speak::Audio> {
        let written=written.to_string();
        let drama3=self.drama3.take();
        let result=(|| -> Result<speak::Audio> {
            let mut all=speak::Audio{samples:vec![],sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.};
            for sentence in speak::drama3::display_sentences(&written) {
                anyhow::ensure!(!c(),"speech interrupted");
                let audio=speak::Synthesizer::synthesize(self,&sentence,l,c)?;
                if c() {break;}
                emit(speak::SpeechPacket::Sentence(sentence))?;
                emit(speak::SpeechPacket::Audio(audio.clone()))?;
                if all.samples.is_empty() {all.sample_rate=audio.sample_rate;all.first_audio_ms=audio.first_audio_ms;}
                all.samples.extend_from_slice(&audio.samples);all.synthesis_ms+=audio.synthesis_ms;
            }
            Ok(all)
        })();
        self.drama3=drama3;
        result
    }
}
/// A line's two voices for `speak_line`: Jarvis stretched to the user's speed, the lighter voice through `synthesize`
/// (Jarvis taken out of the synthesizer meanwhile, so it is never asked twice).
struct LineVoices<'a> { synth: &'a mut LazySynth, language: &'a str, service: &'a speak::drama3::Service }
impl speak::drama3::Voices for LineVoices<'_> {
    fn jarvis(&mut self, service:&speak::drama3::Service, written:&str, resumed:bool, c:&dyn Fn()->bool, emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>)
        -> (Result<speak::Audio>, bool) {
        self.synth.jarvis_packets(service,written,resumed,c,emit)
    }
    fn lighter(&mut self, sentence:&str, c:&dyn Fn()->bool) -> Result<speak::Audio> {
        // Jarvis gave way to free memory: Phonon S (about 300 MB) speaks, as the user asked; Supertonic if it cannot.
        if self.service.memory_held().is_some() && self.synth.voices.is_some() {
            match self.synth.phonon_lighter(sentence,c) {
                Ok(audio)=>return Ok(audio),
                Err(e) if c()=>return Err(e),
                Err(e)=>eprintln!("voice: Phonon S could not say \"{}\" while Jarvis is off for memory, the lighter voice said it: {}",
                    sentence.chars().take(160).collect::<String>(),format!("{e:#}").split_whitespace().collect::<Vec<_>>().join(" ")),
            }
        }
        speak::Synthesizer::synthesize(self.synth,sentence,self.language,c)
    }
    fn jarvis_said(&mut self, text:&str, audio:&speak::Audio) {
        self.synth.record_drama3(text,self.language,audio);
    }
}
impl LazySynth {
    pub(super) fn rate(&self) -> f32 {
        self.speed.load(Ordering::Acquire) as f32 / 100.
    }
    /// Jarvis says `written`, his verified PCM time-stretched to the user's speed on its way to playback (WSOLA, pitch
    /// kept; Jarvis has no rate control of his own); a cut first sentence restarts the stretcher with it. Also
    /// whether playback refused a packet. `resumed`: the rest of a line after other sentences (`stream_resumed`).
    pub(super) fn jarvis_packets(&self, drama3:&speak::drama3::Service, written:&str, resumed:bool, c:&dyn Fn()->bool, emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>)
        -> (Result<speak::Audio>, bool) {
        let rate=self.rate();
        let mut stretcher=speak::stretch::Wsola::new(rate);
        let mut output_failed=false;
        let mut stretched=|packet| {
            let packet=match packet {
                speak::SpeechPacket::Audio(mut audio)=>{
                    audio.samples=stretcher.push(&audio.samples);
                    if audio.samples.is_empty() {return Ok(());}
                    speak::SpeechPacket::Audio(audio)
                },
                speak::SpeechPacket::RestartFirst=>{stretcher=speak::stretch::Wsola::new(rate);speak::SpeechPacket::RestartFirst},
                sentence@speak::SpeechPacket::Sentence(_)=>sentence,
            };
            let result=emit(packet);output_failed |= result.is_err();result
        };
        let result=if resumed {drama3.stream_resumed(written,c,&mut stretched)} else {drama3.stream(written,c,&mut stretched)};
        // The stretcher's last few milliseconds, before completion or before another voice.
        let tail=stretcher.finish();
        if !tail.is_empty() && !output_failed && !c() {
            let flushed=emit(speak::SpeechPacket::Audio(speak::Audio{samples:tail,sample_rate:44100,first_audio_ms:0.,synthesis_ms:0.}));
            if let Err(error)=flushed {return (Err(error),true);}
        }
        (result,output_failed)
    }
    /// The Speed slider's sample, in Jarvis only: a resident Jarvis speaks at once; a loading one is waited for while
    /// the slider row says "Loading Jarvis…"; if he cannot load (memory, a failed start) the row says so and nothing
    /// plays. A Jarvis failure mid-sample ends it; no other voice finishes it.
    pub(super) fn sample_packets(&mut self, sample:&SampleVoice, written:&str, t:&str, l:&str, c:&dyn Fn()->bool, emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>) -> Result<speak::Audio> {
        let drama3=self.drama3.clone().context("Jarvis unavailable")?;
        if !drama3.ready() {
            sample.show(Some("Loading Jarvis…"));
            drama3.wait_ready(SAMPLE_LOAD_WAIT, c);
        }
        if !drama3.ready() {
            if c() { sample.show(None); anyhow::bail!("speech interrupted"); }
            // A short note on the row, cleared unless a newer sample has taken the row since.
            sample.show(Some("Jarvis unavailable"));
            let (generation,mine,later)=(sample.generation.clone(),sample.generation.load(Ordering::Acquire),sample.clone());
            let _=std::thread::Builder::new().name("voice-sample-note".into()).spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(4));
                if generation.load(Ordering::Acquire)==mine { later.show(None); }
            });
            anyhow::bail!("Jarvis unavailable for the voice sample: {}", drama3.status().unwrap_or_else(|| "not loaded".into()));
        }
        sample.show(None);
        let (result,_)=self.jarvis_packets(&drama3,written,false,c,emit);
        let audio=result?;
        self.record_drama3(t,l,&audio);
        Ok(audio)
    }
    pub(super) fn record_drama3(&self,t:&str,l:&str,audio:&speak::Audio) {
        if let Some((bank,origin))=&self.record {
            bank.record(voicebank::ClipMeta {origin:*origin,line_id:None,text:t.into(),language:l.into(),
                voice:speak::drama3::VOICE.into(),generator:speak::drama3::GENERATOR.into(),settings:speak::drama3::settings()},audio.clone());
        }
    }
}
// A queue belongs to the original intent generation, including MCP lifetime.
// Off/On never revives held synthesis, queued jobs or playback in that queue.
pub(super) struct GatedSynth<S> { pub(super) inner: S, pub(super) permit: agent::delivery::DeliveryPermit }
impl<S: speak::Synthesizer> speak::Synthesizer for GatedSynth<S> {
    fn pace(&self) -> f32 { self.inner.pace() }
    fn synthesize(&mut self, text: &str, lang: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.permit.check()?;
        let permit = &self.permit;
        let audio = self.inner.synthesize(text, lang, &|| cancel() || permit.check().is_err())?;
        self.permit.check()?;
        Ok(audio)
    }
    fn stream(&mut self,t:&str,l:&str,c:&dyn Fn()->bool,emit:&mut dyn FnMut(&speak::Audio)->Result<()>) -> Result<speak::Audio> {
        self.permit.check()?;
        let permit=&self.permit;
        let audio=self.inner.stream(t,l,&||c() || permit.check().is_err(),emit)?;
        permit.check()?;
        Ok(audio)
    }
    fn stream_packets(&mut self,t:&str,l:&str,c:&dyn Fn()->bool,emit:&mut dyn FnMut(speak::SpeechPacket)->Result<()>)->Result<speak::Audio> {
        self.permit.check()?;let permit=&self.permit;
        let audio=self.inner.stream_packets(t,l,&||c() || !permit.is_current(),emit)?;
        permit.check()?;Ok(audio)
    }

}
pub(super) struct GatedOutput<O> { pub(super) inner: O, pub(super) permit: agent::delivery::DeliveryPermit }
impl<O: speak::AudioOutput> speak::AudioOutput for GatedOutput<O> {
    fn output(&mut self, audio: &speak::Audio, cancel: &dyn Fn() -> bool) -> Result<()> {
        self.permit.check()?;
        let permit = &self.permit;
        self.inner.output(audio, &|| cancel() || permit.check().is_err())?;
        self.permit.check()
    }
    fn output_stream(&mut self,chunks:speak::PlaybackStream,c:&dyn Fn()->bool,signal:speak::Cancellation)->Result<()> {
        self.permit.check()?;let permit=self.permit.clone();
        let signal=signal.or(move||!permit.is_current());
        let permit=&self.permit;
        self.inner.output_stream(chunks,&||c() || !permit.is_current(),signal)?;
        permit.check()
    }
}
pub(super) struct Speech {
    pub(super) queue: speak::SpeechQueue,
    pub(super) permit: agent::delivery::DeliveryPermit,
}
impl Speech {
    pub(super) fn new(synth: impl speak::Synthesizer, output: impl speak::AudioOutput, permit: agent::delivery::DeliveryPermit) -> Self {
        Self { queue: speak::SpeechQueue::new(
            GatedSynth { inner: synth, permit: permit.clone() },
            GatedOutput { inner: output, permit: permit.clone() }), permit }
    }
}
impl std::ops::Deref for Speech {
    type Target = speak::SpeechQueue;
    fn deref(&self) -> &Self::Target { &self.queue }
}

/// Lighter-voice evidence waiting for the archive, at most; past it a record is dropped and counted. Its tensors wait
/// as 4-byte floats, not JSON numbers (a sentence's vocoder output is a few hundred thousand of them).
const SPEECH_EVIDENCE_QUEUE: usize = 64;
/// The lighter voice's evidence (each model run's outputs and tensors, its errors), written by its own thread. Speech
/// never waits for the archive: on 31 Oct each sentence's tensors were committed on the voice's thread with the
/// archive's 60 s lock wait, so a held lock was 118 s without audio and "lighter voice failed: database is locked".
#[derive(Clone)]
pub(crate) struct SpeechEvidence {
    tx: crossbeam_channel::Sender<(String, u64, serde_json::Value, Vec<Vec<f32>>)>,
    dropped: Arc<AtomicU64>,
}
/// Moves every `tensor_values` array out of `value` into `out` as floats, leaving `tensor_slot` (its index) in place.
fn take_tensors(value: &mut serde_json::Value, out: &mut Vec<Vec<f32>>) {
    if let Some(serde_json::Value::Array(values)) = value.get_mut("tensor_values").map(serde_json::Value::take) {
        let floats: Option<Vec<f32>> = values.iter().map(|v| v.as_f64().map(|x| x as f32)).collect();
        let map = value.as_object_mut().unwrap();
        map.remove("tensor_values");
        match floats {
            Some(floats) => {
                map.insert("tensor_slot".into(), json!(out.len()));
                out.push(floats);
            }
            None => {
                map.insert("tensor_error".into(), json!("invalid tensor value"));
            }
        }
    }
    match value {
        serde_json::Value::Object(map) => map.values_mut().for_each(|c| take_tensors(c, out)),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|c| take_tensors(c, out)),
        _ => {}
    }
}
/// The archive side of `take_tensors`: each slot becomes the vector references `Journal::reference_tensors` writes.
fn put_tensors(value: &mut serde_json::Value, tensors: &[Vec<f32>], journal: &heimdall_archive::Journal) -> Result<()> {
    if let Some(slot) = value.get("tensor_slot").and_then(serde_json::Value::as_u64) {
        let floats = tensors.get(slot as usize).context("tensor slot")?;
        let ids = floats.chunks(32768).map(|v| journal.vector(v)).collect::<Result<Vec<_>>>()?;
        let map = value.as_object_mut().unwrap();
        map.remove("tensor_slot");
        map.insert("vector_ids".into(), json!(ids));
        map.insert("elements".into(), json!(floats.len()));
    }
    match value {
        serde_json::Value::Object(map) => map.values_mut().try_for_each(|c| put_tensors(c, tensors, journal)),
        serde_json::Value::Array(items) => items.iter_mut().try_for_each(|c| put_tensors(c, tensors, journal)),
        _ => Ok(()),
    }
}
impl SpeechEvidence {
    fn start(session: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<(String, u64, serde_json::Value, Vec<Vec<f32>>)>(SPEECH_EVIDENCE_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let lost = dropped.clone();
        let spawned = std::thread::Builder::new().name("heimdall-speech-evidence".into()).spawn(move || {
            let mut journal: Option<heimdall_archive::Journal> = None;
            let mut reported: Option<std::time::Instant> = None;
            for (kind, host_ns, mut payload, tensors) in rx {
                let skipped = lost.swap(0, Ordering::AcqRel);
                if skipped > 0 {
                    payload["dropped_before"] = json!(skipped);
                }
                let written = (|| -> Result<()> {
                    if journal.is_none() {
                        journal = Some(heimdall_archive::Journal::open(&session)?);
                    }
                    let j = journal.as_ref().unwrap();
                    put_tensors(&mut payload, &tensors, j)?;
                    j.event(&kind, "heimdall:synthesis", &[], host_ns, payload)
                })();
                if let Err(e) = written {
                    if reported.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(60)) {
                        eprintln!("Speech evidence not archived ({kind}): {e:#}");
                        reported = Some(std::time::Instant::now());
                    }
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("Speech evidence writer did not start: {e:#}");
        }
        Self { tx, dropped }
    }
    /// Queues one record; never waits. A full queue (the archive's lock held for long) drops it and counts it.
    pub(crate) fn send(&self, kind: &str, mut payload: serde_json::Value) {
        if self.tx.is_full() {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            return;
        }
        let mut tensors = vec![];
        take_tensors(&mut payload, &mut tensors);
        if self.tx.try_send((kind.to_owned(), capture::host_now_ns(), payload, tensors)).is_err() {
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }
}
/// The speech evidence writer of `session`, started on first use.
pub(crate) fn speech_evidence(session: &std::path::Path) -> SpeechEvidence {
    static WRITERS: std::sync::OnceLock<Mutex<std::collections::HashMap<PathBuf, SpeechEvidence>>> = std::sync::OnceLock::new();
    let mut writers = WRITERS.get_or_init(Default::default).lock().unwrap();
    writers.entry(session.to_path_buf()).or_insert_with(|| SpeechEvidence::start(session.to_path_buf())).clone()
}

#[cfg(test)]
mod speech_evidence_tests {
    use super::*;
    /// The queued form keeps every float and writes the same references `Journal::reference_tensors` does.
    #[test]
    fn tensors_wait_as_floats_and_land_as_the_same_vector_references() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let journal = heimdall_archive::Journal::open(&temp.path().join("archive").join("session-speech"))?;
        let original = json!({"model":"supertonic","output":{"tensor_values":[0.5,-0.25,1.0],"shape":[3]},"steps":[{"tensor_values":[2.0]}]});
        let mut expected = original.clone();
        journal.reference_tensors(&mut expected)?;
        let mut queued = original.clone();
        let mut tensors = vec![];
        take_tensors(&mut queued, &mut tensors);
        assert_eq!(tensors, vec![vec![0.5, -0.25, 1.0], vec![2.0]]);
        assert!(!queued.to_string().contains("tensor_values"));
        put_tensors(&mut queued, &tensors, &journal)?;
        assert_eq!(queued, expected);
        Ok(())
    }
}
