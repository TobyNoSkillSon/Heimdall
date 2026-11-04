//! `speak_line`: which voice says each sentence of a line. "sometimes there is the desired Jarvis voice
//! and sometimes it's just some like random voice … the dynamic island actually showed it … make sure that the Jarvis
//! voice is a little bit more stable". A fake worker says every sentence except those containing "Garble" (it gives up
//! after its retries) and stops dead at "Crash"; it logs each request so the tests see what reached Jarvis.
use super::*;
use std::sync::atomic::AtomicUsize;

const WORKER: &str = r#"import sys,json,base64,os,re,time
log=open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'requests.log'),'a')
time.sleep(float(os.environ.get('HEIMDALL_TEST_LOAD','0') or 0))
print(json.dumps({'event':'ready'}),flush=True)
data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*64).decode()
def out(**k): print(json.dumps(k),flush=True)
for line in sys.stdin:
 r=json.loads(line);i=r['id']
 if r['op']!='speak': continue
 log.write(r['text']+('|'+r['direction'] if 'direction' in r else '')+'\n');log.flush()
 parts=[p.strip() for p in re.findall(r'(?:[0-9]\.(?=[0-9])|[^.?!])+[.?!]*|[.?!]+',r['text']) if p.strip()]
 for k,p in enumerate(parts):
  if 'Crash' in p: os._exit(19)
  out(event='pcm',id=i,rate=44100,pcm=data,sentence=k)
  if 'Garble' in p:
   if k==0 and not r.get('buffered'): out(event='restart_first',id=i,sentence=0)
   out(event='fallback',id=i,remaining_sentence=k,synthesis_ms=10);break
  if k>0:
   out(event='sentence_check',id=i,sentence=k,attempt=0)
   v=json.loads(sys.stdin.readline())
   if v['op']=='cancel': out(event='cancelled',id=i);break
   assert v['op']=='verified' and v['accepted'],v
  out(event='sentence_done',id=i,sentence=k)
 else: out(event='done',id=i,synthesis_ms=10,direction=None if 'ignore-direction' in r['text'] else r.get('direction'))
"#;

fn worker(script: &str) -> (tempfile::TempDir, Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    std::fs::write(dir.path().join("sidecar.py"), script).unwrap();
    let service = Service::new(dir.path().into());
    service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
    (dir, service)
}
fn jarvis() -> (tempfile::TempDir, Arc<Service>) {
    let (dir, service) = worker(WORKER);
    service.configure(true);
    assert!(service.wait_ready(Duration::from_secs(5), &|| false), "{:?}", service.events());
    (dir, service)
}
fn requests(dir: &tempfile::TempDir) -> Vec<String> {
    std::fs::read_to_string(dir.path().join("requests.log")).unwrap_or_default().lines().map(str::to_owned).collect()
}
fn switches(service: &Service) -> Vec<(String, String)> {
    service.events().into_iter().filter(|e| e["event"] == "voice_switch")
        .map(|e| (e["text"].as_str().unwrap().to_owned(), e["notice"].as_str().unwrap().to_owned())).collect()
}
/// Jarvis through the service (Jarvis's PCM is 0.2; the lighter voice's 0.8), the lighter voice recorded.
#[derive(Default)]
struct Voices2 {
    lighter: Vec<String>,
    said: Vec<String>,
}
impl Voices for Voices2 {
    fn jarvis(&mut self, service: &Service, written: &str, resumed: bool, cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>) -> (Result<Audio>, bool) {
        let mut refused = false;
        let mut emit = |packet| {
            let result = emit(packet);
            refused |= result.is_err();
            result
        };
        let result = if resumed { service.stream_resumed(written, cancel, &mut emit) } else { service.stream(written, cancel, &mut emit) };
        (result, refused)
    }
    fn lighter(&mut self, sentence: &str, _: &dyn Fn() -> bool) -> Result<Audio> {
        self.lighter.push(sentence.into());
        Ok(Audio { samples: vec![0.8; 10], sample_rate: 44100, first_audio_ms: 0., synthesis_ms: 0. })
    }
    fn jarvis_said(&mut self, text: &str, _: &Audio) {
        self.said.push(text.into());
    }
}
/// The sentences the island was given, each with the voice of the audio that followed it.
fn heard(service: &Service, voices: &mut Voices2, line: &str) -> Result<Vec<(String, &'static str)>> {
    let mut heard: Vec<(String, &'static str)> = vec![];
    speak_line(service, voices, line, &|| false, &mut |packet| {
        match packet {
            crate::SpeechPacket::Sentence(text) => heard.push((text, "")),
            crate::SpeechPacket::Audio(audio) => {
                let voice = if audio.samples.first() == Some(&0.8) { "lighter" } else { "jarvis" };
                let last = heard.last_mut().expect("audio after its sentence");
                assert!(last.1.is_empty() || last.1 == voice, "one voice per sentence: {heard:?}");
                last.1 = voice;
            }
            // Like the app's playback while the user is talking (nothing played yet): a restart drops every unplayed
            // packet of the line, so only a request's own cut first sentence may ever be before it (review 24 Oct).
            crate::SpeechPacket::RestartFirst => heard.clear(),
        }
        Ok(())
    })?;
    Ok(heard)
}

#[test]
fn a_sentence_jarvis_truly_cannot_say_alone_takes_the_lighter_voice_and_jarvis_goes_on() {
    // and 24 Oct 03:29: "Says one sentence". The lighter voice says only the sentence Jarvis cannot say
    // after every retry, then Jarvis says the rest of the line; the switch says why.
    let (dir, service) = jarvis();
    let mut voices = Voices2::default();
    let line = "One is fine. Two is Garble. Three is fine, too. Four is fine.";
    assert_eq!(heard(&service, &mut voices, line).unwrap(), [
        ("One is fine.".to_string(), "jarvis"), ("Two is Garble.".into(), "lighter"),
        ("Three is fine, too.".into(), "jarvis"), ("Four is fine.".into(), "jarvis")]);
    assert_eq!(voices.lighter, ["Two is Garble."]);
    assert_eq!(requests(&dir), [line, "Three is fine, too. Four is fine."]);
    assert_eq!(voices.said, ["One is fine.", "Three is fine too. Four is fine."]);
    let switch = service.events().into_iter().find(|e| e["event"] == "voice_switch").unwrap();
    assert_eq!((switch["text"].as_str(), switch["notice"].as_str()), (Some("Two is Garble."), Some(NOTICE_MISSED)));
    assert_eq!(switch["reason"], format!("incomplete sentence after {ATTEMPTS} takes"), "every fallback says why");
    assert!(service.ready());
    service.stop();
}
#[test]
fn a_failed_first_or_consecutive_sentence_never_moves_the_rest_of_the_line() {
    let (dir, service) = jarvis();
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Garble first. Then fine.").unwrap(),
        [("Garble first.".to_string(), "lighter"), ("Then fine.".into(), "jarvis")]);
    assert_eq!(requests(&dir), ["Garble first. Then fine.", "Then fine."]);
    // Two failed sentences in a row: each alone in the lighter voice, each with its own switch, then Jarvis again.
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Fine. Garble one. Garble two. Fine again. And again.").unwrap(), [
        ("Fine.".to_string(), "jarvis"), ("Garble one.".into(), "lighter"),
        ("Garble two.".into(), "lighter"), ("Fine again.".into(), "jarvis"), ("And again.".into(), "jarvis")]);
    assert_eq!(voices.lighter, ["Garble one.", "Garble two."]);
    assert_eq!(voices.said, ["Fine.", "Fine again. And again."]);
    assert_eq!(&switches(&service)[1..], [
        ("Garble one.".to_string(), NOTICE_MISSED.to_string()), ("Garble two.".into(), NOTICE_MISSED.into())]);
    // A line whose last sentence fails ends in the lighter voice with nothing left for Jarvis.
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Fine. Garble last.").unwrap(),
        [("Fine.".to_string(), "jarvis"), ("Garble last.".into(), "lighter")]);
    assert_eq!(switches(&service), [("Garble last.".to_string(), NOTICE_MISSED.to_string())]);
    service.stop();
}
/// A worker that retries each later sentence three times on the app's word check, as the real one does.
const RETRY_WORKER: &str = r#"import sys,json,base64,os,re
log=open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'requests.log'),'a')
print(json.dumps({'event':'ready'}),flush=True)
data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*64).decode()
def out(**k): print(json.dumps(k),flush=True)
for line in sys.stdin:
 r=json.loads(line);i=r['id']
 if r['op']!='speak': continue
 log.write(r['text']+'\n');log.flush()
 parts=[p.strip() for p in re.findall(r'(?:[0-9]\.(?=[0-9])|[^.?!])+[.?!]*|[.?!]+',r['text']) if p.strip()]
 gave_up=False
 for k,p in enumerate(parts):
  if k==0:
   out(event='pcm',id=i,rate=44100,pcm=data,sentence=0);out(event='sentence_done',id=i,sentence=0);continue
  for a in range(3):
   out(event='pcm',id=i,rate=44100,pcm=data,sentence=k)
   out(event='sentence_check',id=i,sentence=k,attempt=a)
   v=json.loads(sys.stdin.readline())
   if v['accepted']: out(event='sentence_done',id=i,sentence=k);break
   out(event='stall',id=i,sentence=k,attempt=a,reason='word coverage')
   if a<2: out(event='retry',id=i,sentence=k)
  else:
   out(event='fallback',id=i,remaining_sentence=k,reason='word coverage',synthesis_ms=10);gave_up=True;break
 if not gave_up: out(event='done',id=i,synthesis_ms=10)
"#;
/// Jarvis with the retrying worker and a word check that hears each take through `hear(sentence, attempt)`.
fn retrying(hear: impl Fn(&str, usize) -> String + Send + Sync + 'static) -> (tempfile::TempDir, Arc<Service>) {
    let (dir, service) = worker(RETRY_WORKER);
    let takes = Arc::new(Mutex::new((String::new(), 0usize)));
    service.set_verifier(Some(Arc::new(move |text: &str, _: &Audio, _: &dyn Fn() -> bool| {
        let mut takes = takes.lock().unwrap();
        if takes.0 != text || takes.1 == ATTEMPTS as usize {
            *takes = (text.to_owned(), 0);
        }
        let attempt = takes.1;
        takes.1 += 1;
        Ok(crate::coverage::check(text, &hear(text, attempt)))
    })));
    service.configure(true);
    assert!(service.wait_ready(Duration::from_secs(5), &|| false), "{:?}", service.events());
    (dir, service)
}
#[test]
fn a_misheard_name_on_every_take_stays_in_jarviss_voice() {
    // 23 Oct 22:34: "jarovit→yarovit" on all three takes sent the sentence to the lighter voice.
    let (dir, service) = retrying(|text, _| text.replace("Jarovit", "Yarovit"));
    let mut voices = Voices2::default();
    let line = "The Baltic Slavs had a war god. He was called Jarovit. That is all.";
    assert_eq!(heard(&service, &mut voices, line).unwrap(), [
        ("The Baltic Slavs had a war god.".to_string(), "jarvis"), ("He was called Jarovit.".into(), "jarvis"),
        ("That is all.".into(), "jarvis")]);
    assert!(voices.lighter.is_empty());
    assert_eq!(requests(&dir), [line], "his take is kept within the same request");
    let events = service.events();
    assert!(!events.iter().any(|e| e["event"] == "voice_switch"), "{events:?}");
    // Heard alike on two takes: the second is accepted at once (`coverage::accept_agreed_take`), no third take.
    let agreed = events.iter().find(|e| e["event"] == "sentence_coverage" && e["result"]["accepted"] == true && e["sentence"] == 1).expect("logged");
    assert_eq!(agreed["attempt"].as_u64(), Some(1), "{agreed}");
    assert!(agreed["result"]["spellings"].as_array().unwrap().iter().any(|s| s["kind"] == "agreed"), "{agreed}");
    service.stop();
}
#[test]
fn an_earlier_better_take_is_said_when_the_last_one_is_cut() {
    // The first take misheard one word; the later two lost the end of the sentence. Jarvis says the first.
    let (dir, service) = retrying(|text, attempt| match (text.contains("Jarovit"), attempt) {
        (true, 0) => text.replace("Jarovit", "Yarovit"),
        (true, _) => text.replace(" called Jarovit.", "."),
        _ => text.to_owned(),
    });
    let mut voices = Voices2::default();
    let line = "The Slavs had a war god. He was called Jarovit. That is all. Goodbye.";
    assert_eq!(heard(&service, &mut voices, line).unwrap(), [
        ("The Slavs had a war god.".to_string(), "jarvis"), ("He was called Jarovit.".into(), "jarvis"),
        ("That is all.".into(), "jarvis"), ("Goodbye.".into(), "jarvis")]);
    assert!(voices.lighter.is_empty());
    assert_eq!(requests(&dir), [line, "That is all. Goodbye."]);
    assert_eq!(voices.said, ["The Slavs had a war god. He was called Jarovit.", "That is all. Goodbye."]);
    let events = service.events();
    assert!(!events.iter().any(|e| e["event"] == "voice_switch"), "{events:?}");
    let kept = events.iter().find(|e| e["event"] == "take_kept").expect("logged");
    assert_eq!((kept["sentence"].as_u64(), kept["attempt"].as_u64(), kept["errors"].as_u64()), (Some(1), Some(0), Some(1)));
    assert_eq!(kept["reason"], "word coverage");
    // The kept sentence ending the line: nothing left, no second request, no switch.
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Last one. He was called Jarovit.").unwrap(), [
        ("Last one.".to_string(), "jarvis"), ("He was called Jarovit.".into(), "jarvis")]);
    assert_eq!(requests(&dir).len(), 3);
    assert!(!service.events().iter().any(|e| e["event"] == "voice_switch"));
    service.stop();
}
#[test]
fn a_sentence_cut_on_every_take_goes_to_the_lighter_voice_with_its_reason() {
    let (_dir, service) = retrying(|text, _| text.replace(" truly broken.", "."));
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Fine. This one is truly broken. After it.").unwrap(), [
        ("Fine.".to_string(), "jarvis"), ("This one is truly broken.".into(), "lighter"), ("After it.".into(), "jarvis")]);
    assert_eq!(voices.lighter, ["This one is truly broken."]);
    let switch = service.events().into_iter().find(|e| e["event"] == "voice_switch").unwrap();
    assert_eq!(switch["reason"], format!("word coverage after {ATTEMPTS} takes"));
    assert_eq!(switch["text"], "This one is truly broken.");
    service.stop();
}
#[test]
fn a_kept_take_then_a_real_failure_in_one_line_switches_once_for_one_sentence() {
    let (dir, service) = retrying(|text, _| text.replace("Jarovit", "Yarovit").replace(" truly broken.", "."));
    let mut voices = Voices2::default();
    let line = "Fine. He was called Jarovit. This one is truly broken. After it.";
    assert_eq!(heard(&service, &mut voices, line).unwrap(), [
        ("Fine.".to_string(), "jarvis"), ("He was called Jarovit.".into(), "jarvis"),
        ("This one is truly broken.".into(), "lighter"), ("After it.".into(), "jarvis")]);
    assert_eq!(voices.lighter, ["This one is truly broken."]);
    assert_eq!(requests(&dir), [line, "After it."]);
    assert_eq!(switches(&service), [("This one is truly broken.".to_string(), NOTICE_MISSED.to_string())]);
    service.stop();
}
#[test]
fn a_jarvis_that_stops_mid_line_hands_the_rest_to_the_lighter_voice_visibly() {
    let (_dir, service) = jarvis();
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Fine. Crash here. After the crash.").unwrap(), [
        ("Fine.".to_string(), "jarvis"), ("Crash here.".into(), "lighter"), ("After the crash.".into(), "lighter")]);
    assert!(!service.ready());
    assert_eq!(switches(&service), [("Crash here. After the crash.".to_string(), NOTICE_STOPPED.to_string())]);
}
#[test]
fn a_line_while_jarvis_is_not_loaded_is_shown_and_logged_with_the_reason() {
    // 19 Oct 12:15:52: "Yes, I can hear you." while Jarvis waited out a memory refusal was said by the lighter voice
    // with no notice at all.
    let (_dir, service) = worker("import json\nprint(json.dumps({'event':'fatal','reason':'Drama 3 memory guard: available RAM 9.0 GiB, below 14 GiB'}),flush=True)\n");
    service.configure(true);
    assert!(!service.wait_ready(Duration::from_secs(5), &|| false));
    let mut voices = Voices2::default();
    assert_eq!(heard(&service, &mut voices, "Yes, I can hear you.").unwrap(), [("Yes, I can hear you.".to_string(), "lighter")]);
    let switch = service.events().into_iter().find(|e| e["event"] == "voice_switch").expect("shown on the island");
    assert_eq!(switch["notice"], NOTICE_NOT_LOADED);
    assert_eq!(switch["text"], "Yes I can hear you.");
    assert!(switch["reason"].as_str().unwrap().contains("free memory"), "{switch}");
    service.stop();
    // Jarvis not wanted (Speaking off, replay): the lighter voice speaks without a notice.
    let (_dir, idle) = worker(WORKER);
    let mut voices = Voices2::default();
    assert_eq!(heard(&idle, &mut voices, "Replay line.").unwrap(), [("Replay line.".to_string(), "lighter")]);
    assert!(switches(&idle).is_empty());
}
/// Every way a line reaches the voice carries Jarvis while he is loaded: the first line after he loads, several
/// sentences, decimals, Polish, a line cut by an interruption and the line after it, and lines from two queues at
/// once (the reply and the Speed sample share one worker).
#[test]
fn every_line_path_is_said_by_jarvis_while_he_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    // Loads in 0.3 s: the first line arrives while he is loading and waits, as the app does.
    std::fs::write(dir.path().join("sidecar.py"), WORKER.replace("float(os.environ.get('HEIMDALL_TEST_LOAD','0') or 0)", "0.3")).unwrap();
    let service = Service::new(dir.path().into());
    service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
    service.configure(true);
    assert!(service.loading());
    assert!(service.wait_ready(Duration::from_secs(5), &|| false));
    let mut voices = Voices2::default();
    for line in [
        "Ready.",
        "First sentence. Second one, with a comma! Third?",
        "A circle around Opus 5.5 in my window. Version 1.2.3 shipped.",
        "Dzień dobry, User. Wszystko gotowe.",
        "[calm] Tagged <emphasis>words</emphasis> too.",
    ] {
        let heard = heard(&service, &mut voices, line).unwrap();
        assert!(heard.iter().all(|(_, voice)| *voice == "jarvis"), "{line}: {heard:?}");
        assert_eq!(heard.len(), sentences(&normalize(line)).len());
    }
    assert_eq!(heard(&service, &mut voices, "A circle around Opus 5.5 in my window.").unwrap().len(), 1, "a decimal is not a sentence end");
    // Interrupted mid-line, then the next line.
    let cancel = AtomicBool::new(false);
    let result = speak_line(&service, &mut voices, "Hold on. This is cut.", &|| cancel.load(Ordering::Acquire), &mut |_| {
        cancel.store(true, Ordering::Release);
        Ok(())
    });
    assert!(result.is_err());
    assert!(service.ready(), "an interruption keeps Jarvis loaded");
    assert!(heard(&service, &mut voices, "After the interruption.").unwrap().iter().all(|(_, v)| *v == "jarvis"));
    // Two queues at once.
    let lighter = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for line in ["The reply speaks. It has two sentences.", "This is how fast I will speak from now on."] {
            let (service, lighter) = (&service, &lighter);
            scope.spawn(move || {
                let mut voices = Voices2::default();
                let heard = heard(service, &mut voices, line).unwrap();
                assert!(heard.iter().all(|(_, v)| *v == "jarvis"), "{heard:?}");
                lighter.fetch_add(voices.lighter.len(), Ordering::Relaxed);
            });
        }
    });
    assert_eq!(lighter.load(Ordering::Relaxed), 0);
    assert!(voices.lighter.is_empty(), "{:?}", voices.lighter);
    assert!(switches(&service).is_empty());
    service.stop();
}
#[test]
fn decimals_stay_inside_their_sentence_in_both_splitters() {
    assert_eq!(sentences("A circle around Opus 5.5 in my window. A loop."), ["A circle around Opus 5.5 in my window.", "A loop."]);
    assert_eq!(sentences("Version 1.2.3 shipped. Done."), ["Version 1.2.3 shipped.", "Done."]);
    assert_eq!(sentences("It is 5. 5 more to go."), ["It is 5.", "5 more to go."]);
    assert_eq!(sentences("Wait.5 is odd."), ["Wait.", "5 is odd."]);
    assert_eq!(sentences("Price 5..5 now."), ["Price 5..", "5 now."]);
    assert_eq!(display_sentences("Opus 5.5, in my window. Next?"), ["Opus 5.5, in my window.", "Next?"]);
}

/// Ack takes carry their delivery to the worker as its own field; live speech and plain renders never send one.
#[test]
fn directed_render_sends_the_direction_beside_the_plain_text() {
    let (dir, service) = jarvis();
    service.render_directed("Yes, sir.", "crisp and urgent", &|| false).unwrap();
    service.render("Plain.", &|| false).unwrap();
    assert!(service.render_directed("Yes, sir.", "x] [laughs", &|| false).is_err(), "no brackets in a direction");
    assert!(service.render_directed("Plain ignore-direction.", "crisp", &|| false).is_err(), "a worker that ignored it");
    assert_eq!(requests(&dir), ["Yes, sir.|crisp and urgent", "Plain.", "Plain ignore-direction.|crisp"]);
    assert!(valid_direction("dry, quietly amused") && valid_direction("spokojnie i cicho"));
    assert!(!valid_direction("") && !valid_direction("<whisper>") && !valid_direction(&"a".repeat(61)));
}

/// The sidecar runs as "Heimdall Drama 3" when the bundle has that name, so Activity Monitor lists its memory as
/// Heimdall's, not as "python3" (25 Oct); an older bundle keeps python3.
#[test]
fn sidecar_runs_under_heimdalls_name_when_the_bundle_has_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("runtime/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("python3"), "").unwrap();
    assert_eq!(sidecar_executable(dir.path()), bin.join("python3"));
    std::fs::write(bin.join(SIDECAR_PROCESS), "").unwrap();
    assert_eq!(sidecar_executable(dir.path()), bin.join("Heimdall Drama 3"));
}

/// Review 4 Nov: Jarvis gives way to free memory in the middle of a line. Every sentence after it is said while the
/// hold is visible to the lighter voice (the app's `LineVoices` asks per sentence, so Phonon S says the rest, not
/// Supertonic), and the island hears of it once.
#[test]
fn a_give_way_mid_line_leaves_the_rest_of_the_line_to_the_memory_voice() {
    struct Held { service: Arc<Service>, inner: Voices2, seen: Vec<(String, bool)> }
    impl Voices for Held {
        fn jarvis(&mut self, service: &Service, written: &str, resumed: bool, cancel: &dyn Fn() -> bool,
            emit: &mut dyn FnMut(crate::SpeechPacket) -> Result<()>) -> (Result<Audio>, bool) {
            let result = self.inner.jarvis(service, written, resumed, cancel, emit);
            // Memory passes the line while he says this request.
            service.giving_way.store(true, Ordering::Release);
            result
        }
        fn lighter(&mut self, sentence: &str, cancel: &dyn Fn() -> bool) -> Result<Audio> {
            self.seen.push((sentence.into(), self.service.memory_held().is_some()));
            self.inner.lighter(sentence, cancel)
        }
    }
    let (_dir, service) = jarvis();
    let mut voices = Held { service: service.clone(), inner: Voices2::default(), seen: vec![] };
    let line = "One is fine. Two is Garble. Three is fine. Four is fine.";
    speak_line(&service, &mut voices, line, &|| false, &mut |_| Ok(())).unwrap();
    assert_eq!(voices.seen, [("Two is Garble.".to_string(), true), ("Three is fine.".into(), true), ("Four is fine.".into(), true)]);
    let notices: Vec<_> = switches(&service).into_iter().map(|(_, n)| n).collect();
    assert_eq!(notices.iter().filter(|n| *n == NOTICE_MEMORY).count(), 1, "{notices:?}");
    service.stop();
}
