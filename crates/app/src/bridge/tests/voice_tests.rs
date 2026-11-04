//! Heimdall's speech through the app's synthesizer is Jarvis at every speed and in both languages while he is loaded,
//! and a line he cannot take reaches the island as a notice. "sometimes it's just some like random
//! voice … I like the fact that the dynamic island showed it … make sure that the Jarvis voice is a little bit more
//! stable". No Supertonic models exist in these tests, so any use of the lighter voice fails the call.
use super::*;

/// Says every sentence (4,096 samples each), checks every later one, logs each request.
const JARVIS: &str = r#"import sys,json,base64,os,re
log=open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'requests.log'),'a')
print(json.dumps({'event':'ready'}),flush=True)
data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*4096).decode()
def out(**k): print(json.dumps(k),flush=True)
for line in sys.stdin:
 r=json.loads(line);i=r['id']
 if r['op']!='speak': continue
 log.write(r['text']+'\n');log.flush()
 parts=[p for p in re.findall(r'(?:[0-9]\.(?=[0-9])|[^.?!])+[.?!]*|[.?!]+',r['text']) if p.strip()]
 for k,p in enumerate(parts):
  out(event='pcm',id=i,rate=44100,pcm=data,sentence=k)
  if k>0:
   out(event='sentence_check',id=i,sentence=k,attempt=0)
   assert json.loads(sys.stdin.readline())['accepted']
  out(event='sentence_done',id=i,sentence=k)
 out(event='done',id=i,synthesis_ms=10)
"#;

fn service(dir: &Path, script: &str) -> Result<Arc<speak::drama3::Service>> {
    std::fs::create_dir_all(dir.join("runtime/bin"))?;
    std::os::unix::fs::symlink("/usr/bin/python3", dir.join("runtime/bin/python3"))?;
    std::fs::write(dir.join("sidecar.py"), script)?;
    let service = speak::drama3::Service::new(dir.into());
    service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
    Ok(service)
}
fn agent_voice(drama3: &Arc<speak::drama3::Service>, dir: &Path, speed: u16) -> LazySynth {
    LazySynth { drama3: Some(drama3.clone()), session: dir.join("session"), models: dir.join("no-models"), model: Default::default(),
        backend: "auto".into(), record: None, speed: Arc::new(AtomicU16::new(speed)), sample: None, voices: None }
}

#[test]
fn agent_lines_are_jarvis_at_every_speed_and_in_both_languages() -> Result<()> {
    let t = tempfile::tempdir()?;
    let jarvis = service(&t.path().join("jarvis"), JARVIS)?;
    jarvis.configure(true);
    assert!(jarvis.wait_ready(std::time::Duration::from_secs(5), &|| false), "{:?}", jarvis.events());
    let lines = [
        (80, "en", "Noted. The full text stays on the island, with each sentence added."),
        (115, "pl", "Dzień dobry, User. Wszystko gotowe."),
        (160, "en", "A circle around Opus 5.5 in my window."),
        (100, "en", "Worth retrying after the rebuild: a pull-down, and fast reading."),
    ];
    for (speed, language, line) in lines {
        let mut shown = vec![];
        let mut played = 0;
        speak::Synthesizer::stream_packets(&mut agent_voice(&jarvis, t.path(), speed), line, language, &|| false, &mut |packet| {
            match packet {
                speak::SpeechPacket::Sentence(text) => shown.push(text),
                speak::SpeechPacket::Audio(audio) => played += audio.samples.len(),
                speak::SpeechPacket::RestartFirst => panic!("no restart"),
            }
            Ok(())
        })?;
        assert_eq!(shown, speak::drama3::display_sentences(line), "{line}");
        // Jarvis's 4,096 samples a sentence, at the user's speed (pitch-kept stretch).
        let natural = (shown.len() * 4096) as f64;
        let expected = natural * 100. / speed as f64;
        assert!((played as f64 - expected).abs() < natural * 0.2, "{line} at {speed}%: {played} samples, about {expected} expected");
    }
    let requests = std::fs::read_to_string(t.path().join("jarvis/requests.log"))?;
    assert_eq!(requests.lines().collect::<Vec<_>>(), lines.map(|(_, _, line)| speak::drama3::strip_tags(line)));
    assert!(!jarvis.events().iter().any(|e| e["event"] == "voice_switch"));
    jarvis.stop();
    Ok(())
}

#[test]
fn a_line_jarvis_cannot_take_reaches_the_island_before_the_lighter_voice() -> Result<()> {
    // 19 Oct 12:15:52: "Yes, I can hear you." came in the lighter voice while Jarvis waited out a memory refusal, and
    // nothing showed it.
    let t = tempfile::tempdir()?;
    let refused = service(&t.path().join("refused"), "import json\nprint(json.dumps({'event':'memory_refusal','reason':'Not enough free memory for Jarvis right now (needs 31.1 GB, 12.0 GB free)','required_available_bytes':31100000000,'available_bytes':12000000000}),flush=True)\n")?;
    refused.configure(true);
    assert!(!refused.wait_ready(std::time::Duration::from_secs(5), &|| false));
    // The lighter voice has no models here, so the line fails after the switch is recorded.
    assert!(speak::Synthesizer::stream_packets(&mut agent_voice(&refused, t.path(), 115), "Yes, I can hear you.", "en", &|| false, &mut |_| Ok(())).is_err());
    let switch = refused.events().into_iter().find(|e| e["event"] == "voice_switch").expect("the island's notice");
    assert_eq!(switch["notice"], speak::drama3::NOTICE_NOT_LOADED);
    assert!(switch["reason"].as_str().unwrap().starts_with("Not enough free memory for Jarvis"), "{switch}");
    refused.stop();
    Ok(())
}

/// Speaking ▸ the voice (night 1022 menu): with Phonon S chosen, a reply goes sentence by sentence through it (the
/// preset, the island's sentence labels, 44.1 kHz at the user's speed) and never through Jarvis or Supertonic (neither
/// exists here, so either would fail the call).
#[test]
fn a_phonon_choice_speaks_the_reply_through_phonon() -> Result<()> {
    let t = tempfile::tempdir()?;
    let said = Arc::new(Mutex::new(vec![]));
    let voices = Arc::new(crate::speech_voice::tests::fake_voices(said.clone(), Default::default()));
    let mut choice = crate::speech_voice::VoiceChoice::default();
    choice.apply(&ui::speech_voice::SpeechVoiceAction::SetPreset("Everly".into()));
    voices.set_choice(choice);
    let mut synth = LazySynth { drama3: None, session: t.path().join("session"), models: t.path().join("no-models"), model: Default::default(),
        backend: "auto".into(), record: None, speed: Arc::new(AtomicU16::new(160)), sample: None, voices: Some(voices) };
    let mut labels = vec![];
    let mut samples = 0;
    let audio = speak::Synthesizer::stream_packets(&mut synth, "Done. The build is green.", "en", &|| false, &mut |p| {
        match p {
            speak::SpeechPacket::Sentence(s) => labels.push(s),
            speak::SpeechPacket::Audio(a) => { assert_eq!(a.sample_rate, 44_100); samples += a.samples.len(); }
            speak::SpeechPacket::RestartFirst => {}
        }
        Ok(())
    })?;
    assert_eq!(labels, ["Done.", "The build is green."]);
    assert_eq!(said.lock().unwrap().len(), 2);
    assert!(said.lock().unwrap().iter().all(|s| s.starts_with("Everly: ")));
    // 0.2 s per sentence at 160%.
    let seconds = samples as f64 / 44_100.;
    assert!((seconds - 2. * 0.2 / 1.6).abs() < 0.03, "{seconds}");
    assert_eq!(audio.samples.len(), samples);
    // Supertonic chosen: the lighter voice alone, so here (no models) the call fails rather than reaching Phonon.
    let mut choice = crate::speech_voice::VoiceChoice::default();
    choice.apply(&ui::speech_voice::SpeechVoiceAction::SetLocal(ui::speech_voice::LocalVoice::Supertonic));
    synth.voices.as_ref().unwrap().set_choice(choice);
    assert!(speak::Synthesizer::stream_packets(&mut synth, "Hello.", "en", &|| false, &mut |_| Ok(())).is_err());
    assert_eq!(said.lock().unwrap().len(), 2);
    Ok(())
}

/// Editions (crate::edition, docs/editions.md): the public bundle has no Supertonic, so its lighter voice (a line Jarvis
/// or ElevenLabs could not say, an ack-less fallback) is the chosen Phonon S preset; the personal one keeps Supertonic.
/// Runs in both: `cargo test -p app edition` and with `--features app/public`.
#[test]
fn edition_lighter_voice_is_phonon_in_public_and_supertonic_in_personal() -> Result<()> {
    let t = tempfile::tempdir()?;
    let said = Arc::new(Mutex::new(vec![]));
    let voices = Arc::new(crate::speech_voice::tests::fake_voices(said.clone(), Default::default()));
    let mut synth = LazySynth { drama3: None, session: t.path().join("session"), models: t.path().join("no-models"), model: Default::default(),
        backend: "auto".into(), record: None, speed: Arc::new(AtomicU16::new(100)), sample: None, voices: Some(voices) };
    let result = speak::Synthesizer::synthesize(&mut synth, "Hello there.", "en", &|| false);
    if crate::edition::PUBLIC {
        assert!(!result?.samples.is_empty());
        assert_eq!(*said.lock().unwrap(), [format!("{}: Hello there.", speak::phonon::DEFAULT_PRESET)]);
    } else {
        assert!(result.is_err(), "personal: the lighter voice is Supertonic, absent here");
        assert!(said.lock().unwrap().is_empty());
    }
    Ok(())
}
