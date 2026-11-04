//! Speaking ▸ the voice: the saved choice and its migration, routing, ElevenLabs against a fake server, and Phonon S
//! wiring with a fake synthesizer. No model inference, no network beyond 127.0.0.1.
use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use ui::speech_voice::{LocalVoice, SpeechVoiceAction as A};

/// The user's settings as installed builds wrote them before the menu revamp: no `speech_voice`, speed 160%, and the
/// retired `voice` field some older files still carry.
#[cfg(not(feature = "public"))] // personal edition defaults (Drama 3)
#[test]
fn an_old_settings_file_keeps_drama3_at_160_after_install() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("settings.json");
    let mut old = serde_json::to_value(crate::settings::Settings::default())?;
    old.as_object_mut().unwrap().remove("speech_voice");
    old["voice_speed_percent"] = 160.into();
    old["voice"] = "M1".into();
    std::fs::write(&path, serde_json::to_vec(&old)?)?;
    let settings = crate::settings::Settings::load_and_migrate(&path)?;
    assert_eq!(settings.voice_speed_percent, 160);
    assert_eq!(settings.speech_voice, VoiceChoice::default());
    assert_eq!(settings.speech_voice.route(), Route::Jarvis);
    // A fresh install starts there too.
    assert_eq!(crate::settings::Settings::default().speech_voice.route(), Route::Jarvis);
    // A later choice persists.
    let mut next = settings.clone();
    next.speech_voice.apply(&A::SetPreset("Zoey".into()));
    next.save(&path)?;
    let back = crate::settings::Settings::load(&path)?;
    assert_eq!(back.speech_voice.route(), Route::Phonon("Zoey".into()));
    assert_eq!(back.voice_speed_percent, 160);
    // A broken choice is refused like any invalid setting.
    let mut bad = back.clone();
    bad.speech_voice.elevenlabs_voice = Some(ApiVoice { id: "../x".into(), name: "x".into() });
    bad.save(&path)?;
    assert!(crate::settings::Settings::load(&path).is_err());
    Ok(())
}

#[cfg(not(feature = "public"))] // personal edition voices (Supertonic, Jarvis)
#[test]
fn menu_choices_route_local_and_api() {
    let mut c = VoiceChoice::default();
    assert!(c.apply(&A::SetLocal(LocalVoice::Supertonic)));
    assert_eq!(c.route(), Route::Supertonic);
    // API before a voice is chosen keeps speaking with the local choice.
    assert!(c.apply(&A::SetApi(true)));
    assert_eq!(c.route(), Route::Supertonic);
    assert!(c.apply(&A::SetElevenLabsVoice { id: "v1".into(), name: "George".into() }));
    assert_eq!(c.route(), Route::ElevenLabs("v1".into()));
    assert!(c.apply(&A::SetApi(false)));
    assert_eq!(c.route(), Route::Supertonic);
    assert!(c.apply(&A::SetPreset("Hadley".into())));
    assert_eq!(c.route(), Route::Phonon("Hadley".into()));
    assert!(!c.apply(&A::RefreshElevenLabs));
    // Back on API, the saved ElevenLabs voice is still there.
    c.apply(&A::SetApi(true));
    assert_eq!(c.route(), Route::ElevenLabs("v1".into()));
}

/// A one-connection-at-a-time fake of ElevenLabs on 127.0.0.1: `/v1/voices` and the streaming endpoint, refusing any
/// other key with ElevenLabs' error shape. The PCM goes out in odd-sized writes (a sample split across reads).
fn fake_elevenlabs(samples: usize) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let (mut key, mut length) = (String::new(), 0usize);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() { break; }
                let (name, value) = line.split_once(':').unwrap();
                match name.to_ascii_lowercase().as_str() {
                    "xi-api-key" => key = value.trim().into(),
                    "content-length" => length = value.trim().parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(format!("{} {}", request.trim(), String::from_utf8_lossy(&body)));
            let reply = |stream: &mut std::net::TcpStream, status: &str, kind: &str, body: &[u8]| {
                write!(stream, "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).unwrap();
                for part in body.chunks(1001) { stream.write_all(part).unwrap(); stream.flush().unwrap(); }
            };
            if key != "sk_good" {
                reply(&mut stream, "401 Unauthorized", "application/json", br#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#);
            } else if request.starts_with("GET /v1/voices") {
                reply(&mut stream, "200 OK", "application/json", br#"{"voices":[{"voice_id":"JBFqnCBsd6RMkjVDRZzb","name":"George","labels":{"accent":"british","description":"warm"}},{"voice_id":"bad id","name":"Broken"}]}"#);
            } else if request.starts_with("POST /v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb/stream?output_format=pcm_24000") {
                let pcm: Vec<u8> = (0..samples).flat_map(|_| 8192i16.to_le_bytes()).collect();
                reply(&mut stream, "200 OK", "audio/pcm", &pcm);
            } else {
                reply(&mut stream, "404 Not Found", "application/json", b"{}");
            }
        }
    });
    (base, seen)
}

#[test]
fn elevenlabs_lists_voices_streams_pcm_and_reports_a_refused_key() -> Result<()> {
    let (base, seen) = fake_elevenlabs(24_000);
    let client = ElevenLabs::new(&base);
    let voices = client.voices("sk_good")?;
    assert_eq!(voices.iter().map(|v| (v.id.as_str(), v.name.as_str(), v.desc.as_str())).collect::<Vec<_>>(), [("JBFqnCBsd6RMkjVDRZzb", "George", "british, warm")]);
    let refused = client.voices("sk_bad").unwrap_err().to_string();
    assert_eq!(refused, "ElevenLabs: Invalid API key");
    assert!(!refused.contains("sk_bad"));
    // One second of 24 kHz voice through the shared path at 150%: 44.1 kHz packets, two thirds of a second.
    let voices = Voices::with(PathBuf::from("/nonexistent"), Box::new(|_| bail!("no Phonon here")), client, Box::new(|| Some("sk_good".into())));
    let mut packets = vec![];
    let audio = voices.speak_sentence(&Route::ElevenLabs("JBFqnCBsd6RMkjVDRZzb".into()), "Hello there.", "Hello there.", 1.5, &|| false, &mut |p| { packets.push(p); Ok(()) })?;
    assert!(matches!(&packets[0], speak::SpeechPacket::Sentence(s) if s == "Hello there."));
    assert!(packets.iter().skip(1).all(|p| matches!(p, speak::SpeechPacket::Audio(a) if a.sample_rate == 44_100)));
    let seconds = audio.samples.len() as f64 / 44_100.;
    assert!((seconds - 1. / 1.5).abs() < 0.03, "{seconds}");
    assert!(audio.samples.iter().all(|s| (*s - 0.25).abs() < 0.01 || *s == 0.), "the PCM decodes as 16-bit little-endian");
    let posted = seen.lock().unwrap().iter().find(|r| r.starts_with("POST")).cloned().unwrap();
    assert!(posted.ends_with(r#"{"text":"Hello there."}"#), "{posted}");
    // No key: the route fails at once (the speech glue gives the sentence to the lighter voice).
    let keyless = Voices::with(PathBuf::from("/nonexistent"), Box::new(|_| bail!("no")), ElevenLabs::new(&base), Box::new(|| None));
    assert!(keyless.speak_sentence(&Route::ElevenLabs("JBFqnCBsd6RMkjVDRZzb".into()), "Hi.", "Hi.", 1., &|| false, &mut |_| Ok(())).is_err());
    Ok(())
}

/// A stand-in Phonon S: 0.2 s of a constant at 24 kHz per sentence, recording the preset and the text.
pub(crate) struct FakePhonon(pub Arc<Mutex<Vec<String>>>, pub String);
impl speak::voice::StreamingVoice for FakePhonon {
    fn label(&self) -> &str { "fake Phonon" }
    fn stream_sentence(&mut self, text: &str, _: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
        self.0.lock().unwrap().push(format!("{}: {text}", self.1));
        emit(&[0.5; 2_400], 24_000)?;
        emit(&[0.5; 2_400], 24_000)
    }
}
impl PresetVoice for FakePhonon {
    fn set_preset(&mut self, name: &str) -> Result<()> {
        ensure!(name != "Nobody", "no Phonon preset named {name}");
        self.1 = name.into();
        Ok(())
    }
}
pub(crate) fn fake_voices(said: Arc<Mutex<Vec<String>>>, loads: Arc<std::sync::atomic::AtomicUsize>) -> Voices {
    Voices::with(PathBuf::from("/nonexistent"), Box::new(move |_| {
        loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(FakePhonon(said.clone(), String::new())) as Box<dyn PresetVoice>)
    }), ElevenLabs::new("http://127.0.0.1:9"), Box::new(|| None))
}

#[cfg(not(feature = "public"))] // personal edition voices (Supertonic, Jarvis)
#[test]
fn phonon_loads_once_speaks_in_the_preset_and_leaves_memory_when_unchosen() -> Result<()> {
    let said = Arc::new(Mutex::new(vec![]));
    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let voices = fake_voices(said.clone(), loads.clone());
    let mut choice = VoiceChoice::default();
    choice.apply(&A::SetPreset("Zoey".into()));
    voices.set_choice(choice.clone());
    let route = voices.route();
    for text in ["One.", "Two."] {
        voices.speak_sentence(&route, text, text, 1., &|| false, &mut |_| Ok(()))?;
    }
    assert_eq!(*said.lock().unwrap(), ["Zoey: One.", "Zoey: Two."]);
    assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!voices.bank_matches(), "Jarvis's bank lines stay quiet under another voice");
    // An unknown preset fails the sentence (the glue's lighter voice says it).
    assert!(voices.speak_sentence(&Route::Phonon("Nobody".into()), "x.", "x.", 1., &|| false, &mut |_| Ok(())).is_err());
    // Back to Jarvis: the model is dropped, and loads again only when chosen again.
    voices.set_choice(VoiceChoice::default());
    assert!(voices.bank_matches());
    voices.set_choice(choice);
    voices.speak_sentence(&voices.route(), "Three.", "Three.", 1., &|| false, &mut |_| Ok(()))?;
    assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 2);
    Ok(())
}

/// The menu's state carries the bundled presets (the lab copy here) grouped as in voices.json.
#[test]
fn the_menu_state_lists_the_presets_and_what_elevenlabs_needs() {
    // The public bundle's models folder: Phonon S's presets, no Drama 3 beside them.
    let models = tempfile::tempdir().unwrap();
    let lab = models.path().join("phonon");
    std::fs::create_dir_all(&lab).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../speak/phonon/voices.json"), lab.join("voices.json")).unwrap();
    let voices = Voices::with(lab, Box::new(|_| bail!("no")), ElevenLabs::new("http://127.0.0.1:9"), Box::new(|| None));
    let state = voices.ui_state();
    // The suggested voices only, one flat list, Edwin first (the public default).
    assert_eq!(state.presets.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), SUGGESTED_PRESETS);
    assert!(state.presets.iter().all(|p| p.group.is_empty() && !p.desc.is_empty()));
    assert_eq!(state.preset, "Edwin");
    // No Drama 3 beside Phonon S: the public build, where the default (Drama 3) speaks and shows as Phonon S.
    assert!(voices.public() && !state.drama3_installed);
    assert_eq!(state.local, LocalVoice::Phonon);
    assert_eq!(voices.route(), Route::Phonon("Edwin".into()));
    assert!(!state.api);
    assert!(!state.elevenlabs.connected);
    assert_eq!(state.elevenlabs_detail().as_deref(), Some("Not Connected"));
}

/// Night 1022 regression: `Voices` is built inside the async runtime (`Tools::new`); the blocking ElevenLabs client
/// must not be built or dropped there (tokio panics "Cannot drop a runtime in a context where blocking is not
/// allowed", which crashed `pipeline::plumbing_tests::look_respects_seeing…` and would crash the app at startup).
#[tokio::test]
async fn elevenlabs_client_is_safe_to_create_and_drop_inside_the_async_runtime() {
    let client = ElevenLabs::new("http://127.0.0.1:9");
    let copy = client.clone();
    drop(client);
    drop(copy);
}

/// [voices] Own Engine: chosen by its voice, any other choice ends it, Disconnect brings back the voice before it, and
/// the saved file stays readable by the previous build (no new `source` value; the fields it does not know are extra).
#[cfg(not(feature = "public"))] // personal edition voices (Supertonic, Jarvis)
#[test]
fn own_engine_choice_routes_falls_back_and_stays_rollback_readable() {
    use ui::speech_voice::SpeechVoiceAction as A;
    let mut c = VoiceChoice::default();
    assert!(c.apply(&A::SetPreset("Hadley".into())));
    assert!(c.apply(&A::SetOwnVoice { id: "af_heart".into(), name: "Heart".into() }));
    assert_eq!(c.route(), Route::Own("af_heart".into()));
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["source"], "local", "a value the previous build knows");
    assert_eq!((json["own"].as_bool(), json["own_voice"]["id"].as_str()), (Some(true), Some("af_heart")));
    let back: VoiceChoice = serde_json::from_value(json).unwrap();
    assert_eq!(back, c);
    assert!(c.apply(&A::DisconnectOwnEngine));
    assert_eq!(c.route(), Route::Phonon("Hadley".into()), "the voice chosen before speaks again");
    c.apply(&A::SetOwnVoice { id: "af_heart".into(), name: "x".repeat(300) });
    assert!(c.validate().is_ok(), "a long name is cut, not refused");
    assert!(c.apply(&A::SetLocal(ui::speech_voice::LocalVoice::Phonon)));
    assert!(!c.own && c.route() == Route::Phonon("Hadley".into()));
    // A file from before Own Engine loads unchanged.
    let old: VoiceChoice = serde_json::from_str(r#"{"source":"local","local":"drama3","phonon_preset":"Hadley"}"#).unwrap();
    assert_eq!(old.route(), Route::Jarvis);
}

/// Drama 3 only where its model is installed (his personal Mac), and a saved preset outside the
/// suggested ones stays listed so his choice is visible.
#[cfg(not(feature = "public"))] // personal edition defaults (Drama 3)
#[test]
fn drama3_shows_only_where_installed_and_a_saved_preset_stays_listed() {
    let models = tempfile::tempdir().unwrap();
    let phonon = models.path().join("phonon");
    std::fs::create_dir_all(&phonon).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../speak/phonon/voices.json"), phonon.join("voices.json")).unwrap();
    std::fs::create_dir_all(models.path().join("drama3")).unwrap();
    std::fs::write(models.path().join("drama3/model.bin"), b"x").unwrap();
    let voices = Voices::with(phonon, Box::new(|_| bail!("no")), ElevenLabs::new("http://127.0.0.1:9"), Box::new(|| None));
    assert!(!voices.public());
    let state = voices.ui_state();
    assert!(state.drama3_installed && state.local == LocalVoice::Drama3);
    assert_eq!(voices.route(), Route::Jarvis);
    let mut c = VoiceChoice::default();
    c.apply(&A::SetPreset("Zoey".into()));
    voices.set_choice(c);
    let names: Vec<String> = voices.ui_state().presets.iter().map(|p| p.name.clone()).collect();
    assert_eq!(names, ["Edwin", "Spencer", "Preston", "Bryony", "Zoey"]);
}

/// VM finding P-02 (28 Oct): the public edition is Phonon S only even when its models folder is empty (a no-models
/// test build) or a settings file names Drama 3 or Supertonic; the menu never shows Drama 3.
#[cfg(feature = "public")]
#[test]
fn the_public_edition_is_phonon_only_without_any_models_or_with_an_old_drama3_choice() {
    let models = tempfile::tempdir().unwrap();
    let voices = Voices::with(models.path().join("phonon"), Box::new(|_| bail!("no")), ElevenLabs::new("http://127.0.0.1:9"), Box::new(|| None));
    assert!(voices.public() && !voices.drama3_installed());
    assert_eq!(VoiceChoice::default().local, LocalModel::Phonon);
    assert_eq!(voices.ui_state().local, LocalVoice::Phonon);
    for old in [LocalModel::Drama3, LocalModel::Supertonic] {
        voices.set_choice(VoiceChoice { local: old, ..VoiceChoice::default() });
        let state = voices.ui_state();
        assert!(state.local == LocalVoice::Phonon && !state.drama3_installed, "{old:?}");
        assert!(matches!(voices.route(), Route::Phonon(_)), "{old:?}");
    }
}

/// Bug note 2026-11-04T02-17-22: `route` runs per capture tick; the models folder is looked at once per
/// `INSTALLED_EVERY`, not per call (a removal mid-window is not seen until then).
#[cfg(not(feature = "public"))]
#[test]
fn the_route_does_not_list_the_models_folder_per_call() {
    let models = tempfile::tempdir().unwrap();
    let phonon = models.path().join("phonon");
    std::fs::create_dir_all(&phonon).unwrap();
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../speak/phonon/voices.json"), phonon.join("voices.json")).unwrap();
    std::fs::create_dir_all(models.path().join("drama3")).unwrap();
    std::fs::write(models.path().join("drama3/model.bin"), b"x").unwrap();
    let voices = Voices::with(phonon, Box::new(|_| bail!("no")), ElevenLabs::new("http://127.0.0.1:9"), Box::new(|| None));
    assert_eq!(voices.route(), Route::Jarvis);
    std::fs::remove_dir_all(models.path().join("drama3")).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..10_000 {
        assert_eq!(voices.route(), Route::Jarvis, "cached: the folder is not listed again");
    }
    let cached = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..10_000 {
        let _ = std::fs::read_dir(models.path()).is_ok_and(|mut d| d.next().is_some());
    }
    eprintln!("10k routes: {cached:?} cached; 10k folder listings (the old per-call cost): {:?}", start.elapsed());
}

/// Setup's grid: the everyday voices only, Edwin first, then US women, UK women, US men, UK men in the file's order.
#[test]
fn setup_voice_grid_has_no_characters_and_starts_with_edwin() {
    let p = |group: &str, name: &str| speak::phonon::Preset { group: group.into(), name: name.into(), id: name.to_lowercase(), desc: String::new() };
    let all = [p("US women", "Zoey"), p("US men", "Easton"), p("UK women", "Maeve"), p("UK men", "Alfie"), p("UK men", "Edwin"), p("Characters", "Azgorath"), p("US women", "Juniper")];
    let names: Vec<String> = everyday(&all).into_iter().map(|v| v.name).collect();
    assert_eq!(names, ["Edwin", "Zoey", "Juniper", "Maeve", "Easton", "Alfie"]);
}
