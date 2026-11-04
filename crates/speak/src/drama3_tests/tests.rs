    use super::*;
    const FIXTURE: &str = r#"import sys,json,base64,time,os
print(json.dumps({'event':'ready'}),flush=True)
for line in sys.stdin:
 r=json.loads(line);i=r['id']
 if r['op']!='speak': continue
 if r['text']=='pressure': os._exit(73)
 if r['text']=='fail':
  print(json.dumps({'event':'error','id':i,'reason':'fixture stall'}),flush=True);continue
 if r['text']=='Ok. Merged. Then.':
  data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*64).decode()
  print(json.dumps({'event':'pcm','id':i,'sentence':0,'covers':2,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'pcm','id':i,'sentence':0,'covers':2,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0,'covers':2}),flush=True)
  print(json.dumps({'event':'pcm','id':i,'sentence':2,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_check','id':i,'sentence':2,'attempt':0}),flush=True)
  verdict=json.loads(sys.stdin.readline());assert verdict['op']=='verified' and verdict['sentence']==2
  print(json.dumps({'event':'sentence_done','id':i,'sentence':2}),flush=True)
  print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True);continue
 data=base64.b64encode(b'\xcd\xcc\x4c\x3e'*64).decode()
 print(json.dumps({'event':'pcm','id':i,'rate':44100,'pcm':data}),flush=True)
 if r['text'] in ('hold','slow_cleanup'):
  c=json.loads(sys.stdin.readline());assert c=={'op':'cancel','id':i}
  if r['text']=='slow_cleanup': time.sleep(.8)
  print(json.dumps({'event':'cancelled','id':i}),flush=True);continue
 if r['text']=='First. Next.':
  print(json.dumps({'event':'restart_first','id':i,'sentence':0}),flush=True)
  print(json.dumps({'event':'fallback','id':i,'remaining_sentence':0,'synthesis_ms':10}),flush=True);continue
 if r['text'] in ('First. Retry.','First. Reject.','First. Checked.'):
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
  for attempt in range(2):
   print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
   print(json.dumps({'event':'sentence_check','id':i,'sentence':1,'attempt':attempt}),flush=True)
   verdict=json.loads(sys.stdin.readline())
   if verdict['op']=='cancel':
    print(json.dumps({'event':'cancelled','id':i}),flush=True);break
   assert verdict['op']=='verified'
   if verdict['accepted']:
    if r['text']=='First. Checked.': os._exit(19)
    print(json.dumps({'event':'sentence_done','id':i,'sentence':1}),flush=True)
    print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True);break
  else: print(json.dumps({'event':'fallback','id':i,'remaining_sentence':1,'synthesis_ms':10}),flush=True)
  continue
 if r['text']=='First. Late.':
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
  print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_check','id':i,'sentence':1,'attempt':0}),flush=True)
  print(json.dumps({'event':'stall','id':i,'sentence':1,'attempt':0,'reason':'word coverage deadline'}),flush=True)
  print(json.dumps({'event':'retry','id':i,'sentence':1}),flush=True)
  print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_check','id':i,'sentence':1,'attempt':1}),flush=True)
  while True:
   verdict=json.loads(sys.stdin.readline())
   if verdict.get('attempt')==1: break
  if verdict['accepted']:
   print(json.dumps({'event':'sentence_done','id':i,'sentence':1}),flush=True)
   print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True)
  else: print(json.dumps({'event':'fallback','id':i,'remaining_sentence':1,'synthesis_ms':10}),flush=True)
  continue
 if r['text']=='Slow. Check.':
  import select
  for _ in range(3): print(json.dumps({'event':'pcm','id':i,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
  print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_check','id':i,'sentence':1,'attempt':0}),flush=True)
  if not select.select([sys.stdin],[],[],0.8)[0]:
   print(json.dumps({'event':'fallback','id':i,'remaining_sentence':1,'synthesis_ms':10}),flush=True);continue
  sys.stdin.readline()
  print(json.dumps({'event':'sentence_done','id':i,'sentence':1}),flush=True)
  print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True);continue
 if r['text']=='First. Accepted.':
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
  print(json.dumps({'event':'batch','id':i,'first':1,'rows':2,'lead_s':1.2}),flush=True)
  for _ in range(2): print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
  print(json.dumps({'event':'sentence_check','id':i,'sentence':1,'attempt':0}),flush=True)
  verdict=json.loads(sys.stdin.readline());assert verdict['op']=='verified' and verdict['accepted']
  print(json.dumps({'event':'sentence_done','id':i,'sentence':1}),flush=True)
  print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True);continue
 if r['text'] in ('One. Two. Three.','First. Second. Third.'):
  print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
  if r['text']=='First. Second. Third.':
   print(json.dumps({'event':'pcm','id':i,'sentence':1,'rate':44100,'pcm':data}),flush=True)
   os._exit(19)
  print(json.dumps({'event':'fallback','id':i,'remaining_sentence':1,'synthesis_ms':10}),flush=True);continue
 print(json.dumps({'event':'sentence_done','id':i,'sentence':0}),flush=True)
 print(json.dumps({'event':'done','id':i,'synthesis_ms':10}),flush=True)
"#;
    fn fixture() -> (tempfile::TempDir, Arc<Service>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3"))
            .unwrap();
        std::fs::write(dir.path().join("sidecar.py"), FIXTURE).unwrap();
        let service = Service::new(dir.path().into());
        service.configure(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !service.ready() {
            assert!(Instant::now() < deadline, "{:?}", service.events());
            std::thread::sleep(Duration::from_millis(5));
        }
        (dir, service)
    }
    #[test]
    fn a_first_unit_covering_two_sentences_plays_them_as_one_then_checks_the_third() {
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_: &str, _: &Audio, _: &dyn Fn() -> bool| Ok(json!({"accepted": true})))));
        let mut shown = Vec::new();
        let mut audio = 0usize;
        let out = service
            .stream("Ok. Merged. Then.", &|| false, &mut |packet| {
                match packet {
                    crate::SpeechPacket::Sentence(text) => shown.push(text),
                    crate::SpeechPacket::Audio(a) => audio += a.samples.len(),
                    crate::SpeechPacket::RestartFirst => shown.push("RESTART".into()),
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(shown, ["Ok. Merged.", "Then."], "the merged unit shows both sentences, then the third");
        // two live pieces of the merged unit, then the checked third sentence
        assert_eq!(audio, 192);
        assert_eq!(out.samples.len(), 192);
    }

    #[test]
    fn failed_first_retry_cuts_the_prefix_and_falls_back_from_the_first_word() {
        let (_dir, service) = fixture();
        let mut packets = Vec::new();
        let error = service
            .stream("First. Next.", &|| false, &mut |packet| {
                packets.push(match packet {
                    crate::SpeechPacket::Audio(_) => "prefix",
                    crate::SpeechPacket::RestartFirst => "cut",
                    crate::SpeechPacket::Sentence(_) => return Ok(()),
                });
                Ok(())
            })
            .unwrap_err();
        let failure = error.downcast::<SpeechFailure>().unwrap();
        // The prefix may still be waiting to enter playback when the cut arrives; then it is simply never sent.
        assert!(packets == ["prefix", "cut"] || packets == ["cut"], "{packets:?}");
        assert_eq!(failure.remaining, "First. Next.");
        assert!(failure.played.samples.is_empty());
        assert!(failure.played_text.is_empty());
        struct Fallback(Vec<String>);
        impl crate::Synthesizer for Fallback {
            fn synthesize(&mut self, text: &str, _: &str, _: &dyn Fn() -> bool) -> Result<Audio> {
                self.0.push(text.into());
                Ok(Audio {
                    samples: vec![0.8; 10],
                    sample_rate: 44100,
                    first_audio_ms: 0.,
                    synthesis_ms: 0.,
                })
            }
        }
        let mut fallback = Fallback(vec![]);
        let mut complete = 0;
        let audio = finish_unplayed(failure, &mut fallback, "en", &|| false, &mut |a| {
            complete += a.samples.len();
            Ok(())
        })
        .unwrap();
        assert_eq!(fallback.0, ["First.", "Next."]);
        assert_eq!(complete, 20);
        assert_eq!(audio.samples.len(), 20);
        assert!(service.ready());
        service.stop();
    }
    #[test]
    fn a_line_waits_for_a_loading_jarvis_and_a_failed_load_returns_at_once() {
        for (script, ready) in [
            ("import time,json\ntime.sleep(.4)\nprint(json.dumps({'event':'ready','load':{'load_s':.3}}),flush=True)\nimport sys\nsys.stdin.read()\n", true),
            ("import time,json\ntime.sleep(.2)\nprint(json.dumps({'event':'fatal','reason':'no weights'}),flush=True)\n", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
            std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
            std::fs::write(dir.path().join("sidecar.py"), script).unwrap();
            let service = Service::new(dir.path().into());
            service.configure(true);
            assert!(service.loading() && !service.ready());
            let started = Instant::now();
            assert_eq!(service.wait_ready(Duration::from_secs(10), &|| false), ready);
            assert!(started.elapsed() < Duration::from_secs(5));
            assert!(!service.loading());
            if ready {
                let event = service.events().into_iter().find(|e| e["event"] == "ready").unwrap();
                assert!(event["load_ms"].as_f64().unwrap() >= 300.);
                assert_eq!(event["weights_s"], 0.3);
            }
            service.stop();
        }
    }
    #[test]
    fn a_retry_after_a_late_verdict_is_checked_and_played_alone() {
        // 18 Oct: the worker gave up on a verdict, retried, and the client appended the new take to the old one;
        // the doubled audio failed its check and the lighter voice spoke the sentence.
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_, audio, _| Ok(json!({"accepted":audio.samples.len()==64})))));
        let mut played = Vec::new();
        let audio = service
            .stream("First. Late.", &|| false, &mut |packet| {
                if let crate::SpeechPacket::Audio(a) = packet {
                    played.push(a.samples.len());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(played, [64, 64]);
        assert_eq!(audio.samples.len(), 128);
        service.stop();
    }
    #[test]
    fn slow_playback_never_delays_the_workers_verdict() {
        // Each chunk takes 300 ms to enter playback (a full device buffer). The worker waits 0.8 s for its verdict;
        // three accepted first-sentence chunks ahead of it must not hold the check back.
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
        let mut played = 0;
        let audio = service
            .stream("Slow. Check.", &|| false, &mut |packet| {
                if let crate::SpeechPacket::Audio(_) = packet {
                    std::thread::sleep(Duration::from_millis(300));
                    played += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(played, 5);
        assert_eq!(audio.samples.len(), 5 * 64);
        assert!(!service.events().iter().any(|e| e["event"] == "voice_switch"));
        service.stop();
    }
    #[test]
    fn each_sentence_is_announced_as_written_before_its_audio() {
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
        let mut seen = Vec::new();
        service
            .stream("First. Accepted.", &|| false, &mut |packet| {
                seen.push(match packet {
                    crate::SpeechPacket::Sentence(text) => text,
                    crate::SpeechPacket::Audio(_) => "audio".into(),
                    crate::SpeechPacket::RestartFirst => "cut".into(),
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, ["First.", "audio", "Accepted.", "audio", "audio"]);
        let batch = service.events().into_iter().find(|e| e["event"] == "batch").expect("the engine's batch line is kept");
        assert_eq!(batch["rows"], 2);
        assert_eq!(display_sentences("Done: six icons, all shown. Next?"), ["Done: six icons, all shown.", "Next?"]);
        assert_eq!(display_sentences("[calm] Okay. Go."), ["Okay.", "Go."]);
        service.stop();
    }
    #[test]
    fn a_voice_switch_is_recorded_with_the_sentence_it_replaces() {
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":false})))));
        let error = service.stream("First. Reject.", &|| false, &mut |_| Ok(())).unwrap_err();
        assert_eq!(error.downcast::<SpeechFailure>().unwrap().remaining, "Reject.");
        let switch = service.events().into_iter().find(|e| e["event"] == "voice_switch").unwrap();
        assert_eq!(switch["sentence"], 1);
        assert_eq!(switch["text"], "Reject.");
        service.stop();
    }
    #[test]
    fn completed_later_sentence_releases_every_buffered_chunk_once() {
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|text, audio, _| {
            assert_eq!(text, "Accepted.");
            assert_eq!(audio.samples.len(), 128);
            Ok(json!({"accepted":true}))
        })));
        let mut chunks = Vec::new();
        let audio = service
            .stream("First. Accepted.", &|| false, &mut |packet| {
                if let crate::SpeechPacket::Audio(audio) = packet {
                    chunks.push(audio.samples.len());
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(chunks, [64, 64, 64]);
        assert_eq!(audio.samples.len(), 192);
        assert!(service.ready());
        service.stop();
    }
    #[test]
    fn word_check_rejection_never_releases_a_take_before_retry_or_fallback() {
        for accept_retry in [true, false] {
            let (_dir, service) = fixture();
            let calls = Arc::new(AtomicU64::new(0));
            let seen = calls.clone();
            service.set_verifier(Some(Arc::new(move |_, _, _| {
                Ok(json!({"accepted":seen.fetch_add(1,Ordering::Relaxed)==1 && accept_retry}))
            })));
            let mut played = Vec::new();
            let result = service.stream(
                if accept_retry {
                    "First. Retry."
                } else {
                    "First. Reject."
                },
                &|| false,
                &mut |p| {
                    if let crate::SpeechPacket::Audio(a) = p {
                        played.push(a.samples.len());
                    }
                    Ok(())
                },
            );
            assert_eq!(calls.load(Ordering::Relaxed), 2);
            if accept_retry {
                assert_eq!(played, [64, 64]);
                assert_eq!(result.unwrap().samples.len(), 128);
            } else {
                assert_eq!(played, [64]);
                let failure = result.unwrap_err().downcast::<SpeechFailure>().unwrap();
                assert_eq!(failure.remaining, "Reject.");
                assert_eq!(failure.played.samples.len(), 64);
            }
            assert!(service.ready());
            service.stop();
        }
    }
    #[test]
    fn verifier_errors_retry_once_then_fall_back_from_the_unheard_sentence() {
        let (_dir, service) = fixture();
        let calls = Arc::new(AtomicU64::new(0));
        let seen = calls.clone();
        service.set_verifier(Some(Arc::new(move |_, _, _| {
            seen.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("verification deadline")
        })));
        let mut played = Vec::new();
        let error = service
            .stream("First. Reject.", &|| false, &mut |packet| {
                if let crate::SpeechPacket::Audio(audio) = packet {
                    played.push(audio.samples.len());
                }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(played, [64]);
        let failure = error.downcast::<SpeechFailure>().unwrap();
        assert_eq!(failure.remaining, "Reject.");
        assert_eq!(failure.played_text, "First.");
        assert!(service
            .events()
            .iter()
            .filter(|e| e["event"] == "sentence_coverage")
            .all(|e| e["result"]["accepted"] == false));
        service.stop();
    }
    #[test]
    fn verified_audio_still_waits_for_the_workers_completion_marker() {
        let (_dir, service) = fixture();
        service.set_verifier(Some(Arc::new(|_, _, _| Ok(json!({"accepted":true})))));
        let mut chunks = 0;
        let error = service
            .stream("First. Checked.", &|| false, &mut |packet| {
                if matches!(packet, crate::SpeechPacket::Audio(_)) { chunks += 1; }
                Ok(())
            })
            .unwrap_err();
        let failure = error.downcast::<SpeechFailure>().unwrap();
        assert_eq!(chunks, 1);
        assert_eq!(failure.remaining, "Checked.");
        assert_eq!(failure.played.samples.len(), 64);
    }
    #[test]
    fn absent_verifier_fails_closed_from_the_unheard_sentence() {
        let (_dir, service) = fixture();
        let mut chunks = 0;
        let error = service
            .stream("First. Reject.", &|| false, &mut |packet| {
                if matches!(packet, crate::SpeechPacket::Audio(_)) { chunks += 1; }
                Ok(())
            })
            .unwrap_err();
        assert_eq!(chunks, 1);
        assert_eq!(
            error.downcast::<SpeechFailure>().unwrap().remaining,
            "Reject."
        );
        assert!(service
            .events()
            .iter()
            .filter(|v| v["event"] == "sentence_coverage")
            .all(|v| v["result"]["accepted"] == false));
        assert!(service.ready());
        service.stop();
    }
    #[test]
    fn cancel_while_checking_reaches_the_waiting_worker_and_keeps_it_resident() {
        let (_dir, service) = fixture();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        service.set_verifier(Some(Arc::new(move |_, _, _| {
            flag.store(true, Ordering::Release);
            anyhow::bail!("cancelled")
        })));
        assert!(service
            .stream(
                "First. Reject.",
                &|| cancelled.load(Ordering::Acquire),
                &mut |_| Ok(())
            )
            .is_err());
        assert!(service.ready());
        service.stop();
    }
    #[test]
    fn worker_exit_halfway_through_second_sentence_never_plays_or_skips_its_tail() {
        let (_dir, service) = fixture();
        let mut played = Vec::new();
        let error = service
            .stream("First. Second. Third.", &|| false, &mut |packet| {
                if let crate::SpeechPacket::Audio(audio) = packet {
                    played.push(audio.samples.len());
                }
                Ok(())
            })
            .unwrap_err();
        let failure = error.downcast::<SpeechFailure>().unwrap();
        assert_eq!(
            played,
            [64],
            "the partial second sentence must never reach playback"
        );
        assert_eq!(failure.played_text, "First.");
        assert_eq!(failure.remaining, "Second. Third.");
        struct Fallback(Vec<String>);
        impl crate::Synthesizer for Fallback {
            fn synthesize(&mut self, text: &str, _: &str, _: &dyn Fn() -> bool) -> Result<Audio> {
                self.0.push(text.into());
                Ok(Audio {
                    samples: vec![0.8; 64],
                    sample_rate: 44100,
                    first_audio_ms: 0.,
                    synthesis_ms: 0.,
                })
            }
        }
        let mut fallback = Fallback(vec![]);
        finish_unplayed(failure, &mut fallback, "en", &|| false, &mut |audio| {
            played.push(audio.samples.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(fallback.0, ["Second.", "Third."]);
        assert_eq!(played, [64, 64, 64]);
    }
    #[test]
    fn fallback_mid_utterance_never_synthesizes_the_started_sentence_again() {
        let (_dir, service) = fixture();
        let mut output = Vec::new();
        let error = service
            .stream("One. Two. Three.", &|| false, &mut |a| {
                if let crate::SpeechPacket::Audio(a) = a {
                    output.push(a.samples.len());
                }
                Ok(())
            })
            .unwrap_err();
        let failure = error.downcast::<SpeechFailure>().unwrap();
        assert_eq!(failure.remaining, "Two. Three.");
        assert_eq!(failure.played_text, "One.");
        struct Fake {
            calls: Vec<String>,
        }
        impl crate::Synthesizer for Fake {
            fn synthesize(&mut self, t: &str, _: &str, _: &dyn Fn() -> bool) -> Result<Audio> {
                self.calls.push(t.into());
                Ok(Audio {
                    samples: vec![0.2; 64],
                    sample_rate: 44100,
                    first_audio_ms: 0.,
                    synthesis_ms: 1.,
                })
            }
        }
        let mut fallback = Fake { calls: vec![] };
        let audio = finish_unplayed(failure, &mut fallback, "en", &|| false, &mut |a| {
            output.push(a.samples.len());
            Ok(())
        })
        .unwrap();
        assert_eq!(fallback.calls, ["Two.", "Three."]);
        assert_eq!(output, [64, 64, 64]);
        assert_eq!(audio.samples.len(), 192);
        assert!(service.ready());
        service.stop();
    }
    #[test]
    fn admission_refusal_is_recorded_with_readings_and_a_calm_note() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3"))
            .unwrap();
        std::fs::write(dir.path().join("sidecar.py"), "import json,os\nprint(json.dumps({'event':'memory_refusal','reason':'Not enough free memory for Jarvis right now','available_bytes':55000000000,'required_available_bytes':88000000000}),flush=True)\nos._exit(73)\n").unwrap();
        let service = Service::new(dir.path().into());
        service.configure(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        while service.starting.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let events = service.events();
        assert!(events
            .iter()
            .any(|v| v["event"] == "memory_refusal"
                && v["required_available_bytes"] == 88000000000u64));
        assert!(events
            .iter()
            .any(|v| v["event"] == "memory_unload" && v["exit_code"] == 73));
        assert!(!service.ready());
        assert_eq!(
            service.status().as_deref(),
            Some("Not enough free memory for Jarvis right now (needs 88.0 GB, 55.0 GB free)")
        );
    }
    #[test]
    fn memory_refusals_never_spend_crash_budget_and_retry_when_free() {
        let dir=tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/python3",dir.path().join("runtime/bin/python3")).unwrap();
        let sidecar=dir.path().join("sidecar.py");
        std::fs::write(&sidecar,"import json,os\nprint(json.dumps({'event':'memory_refusal','reason':'Not enough free memory for Jarvis right now','available_bytes':28000000000,'required_available_bytes':31147483648}),flush=True)\nos._exit(73)\n").unwrap();
        let s=Service::new(dir.path().into());
        for _ in 0..4 {
            s.configure(true);
            let deadline=Instant::now()+Duration::from_secs(3);
            while s.starting.load(Ordering::Acquire) {
                assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));
            }
            assert!(s.failures.lock().unwrap().is_empty());
            assert!(s.policy_waiting());
            assert!(s.status().unwrap().contains("needs 31.1 GB, 28.0 GB free"));
            // Advance the policy retry clock, not the failure budget.
            *s.memory_wait.lock().unwrap()=Some(Instant::now()-Duration::from_secs(31));
        }
        std::fs::write(&sidecar,"import json,sys\nprint(json.dumps({'event':'ready'}),flush=True)\nsys.stdin.read()\n").unwrap();
        s.configure(true);
        let deadline=Instant::now()+Duration::from_secs(3);
        while s.starting.load(Ordering::Acquire) {
            assert!(Instant::now()<deadline);std::thread::sleep(Duration::from_millis(5));
        }
        assert!(s.ready());assert!(!s.policy_waiting());assert!(s.status().is_none());s.stop();
    }
    #[test]
    fn a_retiring_verifiers_reserve_reaches_readmission_and_is_diagnostic() {
        let (dir, service) = fixture();
        service.stop();
        std::fs::write(dir.path().join("sidecar.py"),FIXTURE.replace("print(json.dumps({'event':'ready'}),flush=True)","assert int(os.environ['HEIMDALL_DRAMA3_RETAINED_VERIFIER_BYTES'])==2000000000\nprint(json.dumps({'event':'ready'}),flush=True)")).unwrap();
        service.set_verifier_reserve(Arc::new(|| 2_000_000_000));
        service.record_verifier_unload(2_000_000_000);
        service.wanted.store(true, Ordering::Release);
        service.start().unwrap();
        service.render("ready", &|| false).unwrap();
        let events = service.events();
        for event in ["verifier_unload_pending", "verifier_reserve_carried"] {
            assert!(events
                .iter()
                .any(|v| v["event"] == event && v["retained_bytes"] == 2_000_000_000u64));
        }
        service.stop();
    }
    #[test]
    fn memory_exit_without_protocol_is_diagnostic_and_has_a_calm_note() {
        let (_dir, service) = fixture();
        assert!(service
            .stream("pressure", &|| false, &mut |_| Ok(()))
            .is_err());
        assert!(service
            .events()
            .iter()
            .any(|v| v["event"] == "memory_unload" && v["exit_code"] == 73));
        assert_eq!(
            service.status().as_deref(),
            Some("Jarvis unloaded to protect free memory; retrying when memory is available.")
        );
    }
    #[test]
    fn cancellation_deadline_includes_delayed_cleanup() {
        let (_dir, service) = fixture();
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        assert!(service
            .stream(
                "slow_cleanup",
                &|| cancel.load(Ordering::Acquire),
                &mut |_| {
                    cancel.store(true, Ordering::Release);
                    Ok(())
                }
            )
            .is_err());
        assert!(started.elapsed() >= Duration::from_millis(490));
        assert!(started.elapsed() < Duration::from_millis(780));
        assert!(!service.ready());
        assert_eq!(service.pid.load(Ordering::Acquire), 0);
        // An interruption the worker left too slowly is not a voice failure: no back-off, Jarvis reloads at once
        // and the next line can wait for him (21 Oct 16:42: half a minute of the lighter voice after such a kill).
        assert!(service.failures.lock().unwrap().is_empty());
        assert!(service.events().iter().any(|v| v["event"] == "interrupted_restart"));
        service.resume();
        assert!(service.wait_ready(Duration::from_secs(5), &|| false), "{:?}", service.events());
        service.stop();
    }
    #[test]
    fn a_worker_that_keeps_missing_the_cancel_deadline_counts_as_failing() {
        let (_dir, service) = fixture();
        for _ in 0..INTERRUPTED_RESTARTS {
            service.interrupted("Drama 3 cancellation deadline".into());
        }
        assert!(service.failures.lock().unwrap().is_empty());
        service.interrupted("Drama 3 cancellation deadline".into());
        assert_eq!(service.failures.lock().unwrap().len(), 1, "wedged, not interrupted");
        // `resume` never claims Jarvis is wanted: after Speaking turns off it starts nothing.
        service.configure(false);
        service.resume();
        assert!(!service.loading() && !service.wanted.load(Ordering::Acquire));
    }
    #[test]
    fn verifier_residency_follows_ready_stop_failure_and_drop() {
        let (_dir, service) = fixture();
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        service.set_residency_hook(Arc::new(move |ready| seen.lock().unwrap().push(ready)));
        assert_eq!(*events.lock().unwrap(), [true]);
        service.render("fail", &|| false).unwrap_err();
        assert_eq!(*events.lock().unwrap(), [true, false]);
        service.stop();
        assert_eq!(events.lock().unwrap().last(), Some(&false));
        drop(service);
        assert_eq!(events.lock().unwrap().last(), Some(&false));
    }
    #[test]
    fn a_stale_ready_notification_after_stop_never_reloads_verification() {
        let (_dir, service) = fixture();
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        service.set_residency_hook(Arc::new(move |ready| seen.lock().unwrap().push(ready)));
        service.stop();
        service.notify_residency(true);
        assert_eq!(*events.lock().unwrap(), [true, false, false]);
        assert!(!service.ready());
    }
    #[test]
    fn normalizes_tags_punctuation_and_apostrophes() {
        assert_eq!(
            normalize("[calm] <speak>I'm ready, User: it’s done — tak?</speak>"),
            "I'm ready User it's done tak?"
        );
        assert_eq!(normalize("[sad]<break/>"), "");
    }
    /// The same table as `test_sidecar.py` `test_symbols_are_spelled`.
    #[test]
    fn symbols_are_spelled_as_said() {
        for (written, said) in [
            ("Zażółć, gęślą! (Test) 42% + $5.", "Zażółć gęślą! Test 42 procent plus 5 dolarów."),
            ("Background jobs now cost about 40% less.", "Background jobs now cost about 40 percent less."),
            ("About 18,500 dollars, or $3,100 a month, 2,500,000 in all.", "About 18500 dollars or 3100 dollars a month 2500000 in all."),
            ("$1 today, $1.5 billion later, €20k and £3M.", "1 dollar today 1.5 billion dollars later 20 thousand euros and 3 million pounds."),
            ("A 1920x1080 screen, 4x faster, 0x1F stays.", "A 1920 by 1080 screen 4 times faster 0x1F stays."),
            ("Salt & pepper, C++ at 20°C or 68 °F, #1, a@b.", "Salt and pepper C plus plus at 20 degrees Celsius or 68 degrees Fahrenheit number 1 a at b."),
            ("A 4K screen with 20k rows and 600M weights, 1, 2, 3.", "A 4K screen with 20 thousand rows and 600 million weights 1 2 3."),
            ("Turn 90° to the left, 1° off, 1°C.", "Turn 90 degrees to the left 1 degree off 1 degree Celsius."),
            ("$5B and $5m, 20€ or 20 € or 1 $.", "5 billion dollars and 5 million dollars 20 euros or 20 euros or 1 dollar."),
            ("0,125 and 1,234,5 stay apart.", "0 125 and 1 234 5 stay apart."),
            ("A 1920X1080 screen, a 4k video, a 5k run, #1a1a1a.", "A 1920 by 1080 screen a 4k video a 5k run 1a1a1a."),
            ("Kosztuje 22 $, 1 € i 2,5 zł, 40% mniej, 2 x 3, 5°C & #2.", "Kosztuje 22 dolary 1 euro i 2 5 złotego 40 procent mniej 2 na 3 5 stopni Celsjusza i numer 2."),
            ("Mam 12 $ i 5 zł oraz 3,100 słów.", "Mam 12 dolarów i 5 złotych oraz 3 100 słów."),
            ("Flights to Kraków are 40% cheaper, 1,500 a day.", "Flights to Kraków are 40 percent cheaper 1500 a day."),
            ("Python 3 x64 on Windows 10 x64, a 2 x 3 grid, buy 2x 4090.", "Python 3 x64 on Windows 10 x64 a 2 by 3 grid buy 2 times 4090."),
            ("Zebrał $2k, $1k, $3M, 22k użytkowników, 2M zł i 5k zł.", "Zebrał 2 tysiące dolarów 1 tysiąc dolarów 3 miliony dolarów 22 tysiące użytkowników 2 miliony złotych i 5 tysięcy złotych."),
            ("Model 1.7B on an M5, version 5.5, at 00:40, call +48 123.", "Model 1.7B on an M5 version 5.5 at 00 40 call plus 48 123."),
        ] {
            assert_eq!(normalize(written), said, "{written}");
        }
    }
    /// Ranges and on-the-hour times (`ranges`, Rust only: the worker is sent the plain words). 4 Nov 02:15: "$100–$300"
    /// was said "100 dollars 300 dollars"; 3 Nov 03:28: "15:00" was said as "15 00".
    #[test]
    fn ranges_and_times_are_said_as_spoken() {
        for (written, said) in [
            ("Charge $100–$300 per client.", "Charge 100 to 300 dollars per client."),
            ("Charge $100 to $300, or $5-$10.", "Charge 100 to 300 dollars or 5 to 10 dollars."),
            ("Roughly $19k–$42k a month, €5 to $9.", "Roughly 19 thousand to 42 thousand dollars a month 5 euros to 9 dollars."),
            ("Plans at $12 and $25, $5B and $5m.", "Plans at 12 dollars and 25 dollars 5 billion dollars and 5 million dollars."),
            ("Down 10%-20% and 3–4 hours, a 4–1 win.", "Down 10 percent to 20 percent and 3 to 4 hours a 4 to 1 win."),
            ("Cancelled the session at 15:00, moved to 9:00, until 12:00, from 00:00.", "Cancelled the session at 3 PM moved to 9 o'clock until noon from midnight."),
            // Not a time of day after a preposition or "the": as before.
            ("At 12:00 AM, at 1:00:00, it took 2:00 to load.", "At 12 00 AM at 1 00 00 it took 2 00 to load."),
            ("I've cancelled the 15:00 session.", "I've cancelled the 3 PM session."),
            ("In 2024 — 3 years ago — we started.", "In 2024 3 years ago we started."),
            ("At 15:30 and 00:40.", "At 15 30 and 00 40."),
            ("Qwen3-8B on 2026-11-04 with GPT-4o.", "Qwen3 8B on 2026 11 04 with GPT 4o."),
            ("Kosztuje 100–300 zł o 15:00.", "Kosztuje 100 do 300 złotych o 15 00."),
        ] {
            assert_eq!(normalize(written), said, "{written}");
            assert_eq!(normalize(&normalize(written)), said, "the worker's second pass changes nothing: {written}");
        }
    }
    #[test]
    fn streams_reuses_process_and_cancels_without_retiring_the_worker() {
        let (_dir, service) = fixture();
        let pid = service.pid.load(Ordering::Acquire);
        let mut chunks = 0;
        let audio = service
            .stream("Okay.", &|| false, &mut |a| {
                match a {
                    crate::SpeechPacket::Audio(a) => {
                        chunks += 1;
                        assert_eq!(a.samples.len(), 64);
                    }
                    crate::SpeechPacket::Sentence(text) => assert_eq!(text, "Okay."),
                    crate::SpeechPacket::RestartFirst => panic!("no restart"),
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(chunks, 1);
        assert_eq!(audio.samples.len(), 64);
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        assert!(service
            .stream("hold", &|| cancel.load(Ordering::Acquire), &mut |_| {
                cancel.store(true, Ordering::Release);
                Ok(())
            })
            .is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(service.ready());
        service.render("Again.", &|| false).unwrap();
        assert_eq!(service.pid.load(Ordering::Acquire), pid);
        service.stop();
        assert!(!service.ready());
        assert_eq!(service.pid.load(Ordering::Acquire), 0);
    }
    #[test]
    fn playback_failure_cancels_generation_and_keeps_the_resident_worker() {
        let (_dir, service) = fixture();
        let error = service
            .stream("hold", &|| false, &mut |_| {
                anyhow::bail!("output device failed")
            })
            .unwrap_err();
        assert!(error.to_string().contains("output device failed"));
        assert!(service.ready());
        service.render("Again.", &|| false).unwrap();
        service.stop();
    }
    #[test]
    fn speaking_off_during_a_request_does_not_spend_restart_budget_or_leave_a_failure_note() {
        let (_dir, service) = fixture();
        let (tx, rx) = mpsc::channel();
        let other = service.clone();
        let worker = std::thread::spawn(move || {
            other.stream("hold", &|| false, &mut |_| {
                tx.send(()).unwrap();
                Ok(())
            })
        });
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        service.stop();
        assert!(worker.join().unwrap().is_err());
        assert!(!service.ready());
        assert!(service.status().is_none());
        assert!(service.failures.lock().unwrap().is_empty());
        assert_eq!(service.pid.load(Ordering::Acquire), 0);
    }
    #[test]
    fn serializes_competing_callers_and_records_errors() {
        let (_dir, service) = fixture();
        let other = service.clone();
        let thread =
            std::thread::spawn(move || other.render("One.", &|| false).unwrap().samples.len());
        assert_eq!(service.render("Two.", &|| false).unwrap().samples.len(), 64);
        assert_eq!(thread.join().unwrap(), 64);
        assert!(service.render("fail", &|| false).is_err());
        assert!(!service.ready());
        assert!(service.events().iter().any(|v| v["event"] == "failure"));
        assert_eq!(service.pid.load(Ordering::Acquire), 0);
    }
    #[test]
    fn restart_budget_is_bounded_and_unavailable_runtime_stays_nonblocking() {
        let dir = tempfile::tempdir().unwrap();
        let s = Service::new(dir.path().into());
        for _ in 0..3 {
            s.configure(true);
            let deadline = Instant::now() + Duration::from_secs(2);
            while s.starting.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            *s.failures.lock().unwrap().back_mut().unwrap() =
                Instant::now() - Duration::from_secs(31);
        }
        s.configure(true);
        assert!(!s.starting.load(Ordering::Acquire));
        assert_eq!(s.failures.lock().unwrap().len(), 3);
        assert!(s.status().unwrap().contains("repeated failures"));
        s.stop();
        assert!(s.status().is_none());
    }
/// 19 Oct 18:55:47: Jarvis left while idle (exit 73) and nothing recorded why. The guard's last line is kept.
#[test]
fn an_idle_exit_keeps_the_workers_reason() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    std::fs::write(dir.path().join("sidecar.py"), "import json,os,time\nprint(json.dumps({'event':'ready'}),flush=True)\ntime.sleep(.2)\nprint(json.dumps({'event':'fatal','reason':'Swap grew by 54 MB with 20.1 GB free. Using the lighter voice.','available_bytes':20100000000,'swap_growth_bytes':54000000}),flush=True)\nos._exit(73)\n").unwrap();
    let service = Service::new(dir.path().into());
    service.configure(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.ready() {
        assert!(Instant::now() < deadline, "{:?}", service.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(400));
    service.configure(true);
    let events = service.events();
    let unload = events.iter().find(|v| v["event"] == "memory_unload").expect("memory_unload");
    assert_eq!(unload["phase"], "idle");
    assert_eq!(unload["reason"], "Swap grew by 54 MB with 20.1 GB free. Using the lighter voice.");
    assert!(events.iter().any(|v| v["event"] == "fatal" && v["swap_growth_bytes"] == 54000000), "{events:?}");
    assert!(events.iter().any(|v| v["event"] == "memory_wait" && v["reason"].as_str().unwrap().contains("Swap grew by 54 MB")), "{events:?}");
    assert!(!service.ready());
}
/// 19 Oct 16:47: the engine's own "Need 55 GB available" refusal exits 73 like every memory refusal; it waits for
/// memory instead of spending the three-per-five-minutes crash budget.
#[test]
fn an_engine_memory_refusal_at_load_is_a_memory_wait_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    std::fs::write(dir.path().join("sidecar.py"), "import json,os\nprint(json.dumps({'event':'fatal','reason':'Need 55 GB available before loading Drama 3'}),flush=True)\nos._exit(73)\n").unwrap();
    let s = Service::new(dir.path().into());
    for _ in 0..4 {
        s.configure(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        while s.starting.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(s.failures.lock().unwrap().is_empty(), "{:?}", s.events());
        assert!(s.policy_waiting());
        *s.memory_wait.lock().unwrap() = Some(Instant::now() - Duration::from_secs(31));
    }
    assert!(s.events().iter().any(|v| v["event"] == "memory_wait" && v["reason"].as_str().unwrap().contains("Need 55 GB")));
}
fn sidecar(script: &str) -> (tempfile::TempDir, Arc<Service>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    std::fs::write(dir.path().join("sidecar.py"), script).unwrap();
    let service = Service::new(dir.path().into());
    (dir, service)
}
fn settle(s: &Service) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.starting.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "{:?}", s.events());
        std::thread::sleep(Duration::from_millis(5));
    }
}
/// 26 Oct 01:38: one sidecar load failure sent Jarvis's lines to the lighter voice. The load is tried once more first.
#[test]
fn a_failed_load_is_retried_once_before_the_lighter_voice() {
    let (_dir, s) = sidecar("import json,os,sys\nm=os.path.join(os.path.dirname(os.path.abspath(__file__)),'tried')\nif not os.path.exists(m):\n open(m,'w').close();print(json.dumps({'event':'fatal','reason':'Too many open files (os error 24)'}),flush=True);sys.exit(1)\nprint(json.dumps({'event':'ready'}),flush=True)\nsys.stdin.read()\n");
    s.configure(true);
    settle(&s);
    assert!(s.ready(), "{:?}", s.events());
    let events = s.events();
    let retry = events.iter().find(|v| v["event"] == "load_retry").expect("load_retry");
    assert_eq!(retry["cause"], "fd_exhausted");
    assert!(!events.iter().any(|v| v["event"] == "failure"), "{events:?}");
    assert!(s.failures.lock().unwrap().is_empty());
    s.stop();
}
/// A load that fails twice is one counted failure, and the "not loaded" switch that follows carries its cause and error
/// rather than the status note.
#[test]
fn a_switch_after_a_failed_load_carries_the_failures_cause() {
    let (_dir, s) = sidecar("import json\nprint(json.dumps({'event':'fatal','reason':'Too many open files (os error 24)'}),flush=True)\n");
    s.configure(true);
    settle(&s);
    let events = s.events();
    assert_eq!(events.iter().filter(|v| v["event"] == "load_retry").count(), 1, "{events:?}");
    assert_eq!(events.iter().filter(|v| v["event"] == "failure").count(), 1, "{events:?}");
    assert_eq!(s.failures.lock().unwrap().len(), 1);
    s.lighter_voice("Noted.", NOTICE_NOT_LOADED, &s.status().unwrap());
    let switch = s.events().into_iter().find(|v| v["event"] == "voice_switch").unwrap();
    assert_eq!((switch["cause"].as_str(), switch["from"].as_str(), switch["to"].as_str()), (Some("fd_exhausted"), Some("jarvis"), Some("lighter")));
    assert!(switch["error"].as_str().unwrap().contains("os error 24"), "{switch}");
    assert_eq!(switch["text"], "Noted.");
    s.stop();
}
#[test]
fn fallback_causes_group_the_logged_reasons() {
    for (reason, cause) in [
        ("Too many open files (os error 24)", "fd_exhausted"),
        ("sidecar load: channel is empty and sending half is closed", "load_failed"),
        ("sidecar exited exit status: 19: no reason given", "sidecar_crash"),
        ("Drama 3 load deadline", "timeout"),
        ("word coverage deadline", "timeout"),
        ("incomplete sentence after 3 takes", "word_check"),
        ("Not enough free memory for Jarvis right now (needs 31.1 GB, 15.7 GB free)", "memory"),
        ("Drama 3 unavailable. Using the lighter voice.", "load_failed"),
        ("Jarvis voice loading", "not_loaded"),
        ("speech interrupted", "other"),
        ("mlx is not installed", "other"),
        ("the live take stalled", "timeout"),
        ("word coverage after 3 takes", "word_check"),
    ] {
        assert_eq!(fallback_cause(reason), cause, "{reason}");
    }
    assert!(!retryable_load("sidecar load: {\"event\":\"memory_refusal\"}; Memory unload (exit 73)"));
    assert!(!retryable_load("Drama 3 runtime missing: /x"));
    assert!(!retryable_load("Drama 3 load deadline"));
    assert!(retryable_load("sidecar load: channel is empty and sending half is closed"));
}

/// Gone from the process table: reaped, not a zombie (`kill(pid, 0)` still finds a zombie).
fn reaped_within(pid: libc::pid_t, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        if unsafe { libc::kill(pid, 0) } == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}
/// 24 Oct 23:13: Speaking off waited 2.2 s on the live loop for Jarvis to exit. A child that takes a second to
/// leave stands in for a process tearing down a ~31 GB model: the caller returns at once and the child is reaped.
#[test]
fn a_slow_exit_is_reaped_off_the_calling_thread() {
    let child = std::process::Command::new("/bin/sleep").arg("1").spawn().unwrap();
    let pid = child.id() as libc::pid_t;
    let at = Instant::now();
    reap_detached(pid);
    assert!(at.elapsed() < Duration::from_millis(50), "waited {:?}", at.elapsed());
    assert!(reaped_within(pid, Duration::from_secs(5)), "left a zombie");
    drop(child);
}
#[test]
fn dropping_a_process_kills_it_and_leaves_no_zombie() {
    let mut child = std::process::Command::new("/bin/sleep").arg("3")
        .stdin(std::process::Stdio::piped()).spawn().unwrap();
    let pid = child.id() as i32;
    let input = child.stdin.take().unwrap();
    let shared = Arc::new(AtomicI32::new(pid));
    let (_tx, output) = mpsc::channel();
    let at = Instant::now();
    drop(Process { pid: shared.clone(), child, input, output });
    assert!(at.elapsed() < Duration::from_millis(50), "drop waited {:?}", at.elapsed());
    assert_eq!(shared.load(Ordering::Acquire), 0, "the pid is cleared");
    assert!(reaped_within(pid, Duration::from_secs(2)), "left a zombie");
}

#[test]
fn rust_engine_by_default_when_bundled_and_python_only_by_the_switch() {
    let assets = std::env::temp_dir().join(format!("drama3-rust-switch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&assets);
    std::fs::create_dir_all(assets.join("rust")).unwrap();
    // no binary in the bundle: the Python worker, whatever the switch says
    assert_eq!(super::rust_engine_for(&assets, None), None);
    assert_eq!(super::rust_engine_for(&assets, Some("rust")), None);
    std::fs::write(assets.join("rust").join(super::SIDECAR_PROCESS), b"").unwrap();
    // binary present: the Rust engine, under Heimdall's process name, unless the switch says python
    let rust = Some(assets.join("rust").join("Heimdall Drama 3"));
    assert_eq!(super::rust_engine_for(&assets, None), rust);
    assert_eq!(super::rust_engine_for(&assets, Some("rust")), rust);
    assert_eq!(super::rust_engine_for(&assets, Some("")), rust);
    assert_eq!(super::rust_engine_for(&assets, Some("python")), None);
    std::fs::remove_dir_all(&assets).unwrap();
}

/// A bundle whose Python worker says ready and whose Rust engine is `rust` (a shell script).
fn engines(rust: &str) -> (tempfile::TempDir, Arc<Service>) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime/bin")).unwrap();
    std::fs::create_dir_all(dir.path().join("rust")).unwrap();
    std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("runtime/bin/python3")).unwrap();
    std::fs::write(dir.path().join("sidecar.py"), "import json,sys\nprint(json.dumps({'event':'ready'}),flush=True)\nfor _ in sys.stdin: pass\n").unwrap();
    let engine = dir.path().join("rust").join(super::SIDECAR_PROCESS);
    std::fs::write(&engine, format!("#!/bin/sh\n{rust}\n")).unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    let service = Service::new(dir.path().into());
    (dir, service)
}

fn ready_engine(service: &Arc<Service>) -> Vec<Value> {
    service.configure(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.ready() {
        assert!(Instant::now() < deadline, "{:?}", service.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    service.events()
}

#[test]
fn the_rust_engine_serves_when_it_starts() {
    let (_dir, service) = engines("echo '{\"event\":\"ready\"}'; cat >/dev/null");
    let events = ready_engine(&service);
    assert!(events.iter().any(|e| e["event"] == "ready" && e["engine"] == "rust"), "{events:?}");
    assert!(!events.iter().any(|e| e["event"] == "engine_fallback"), "{events:?}");
    service.stop();
}

#[test]
fn a_rust_engine_that_fails_to_start_hands_over_to_the_python_worker_for_good() {
    let (_dir, service) = engines("echo '{\"event\":\"fatal\",\"reason\":\"Metal library missing\"}'; exit 1");
    let events = ready_engine(&service);
    let fallback = events.iter().position(|e| e["event"] == "engine_fallback").expect("a fallback event");
    assert_eq!(events[fallback]["from"], "rust");
    assert!(events[fallback]["reason"].as_str().unwrap().contains("Metal library missing"), "{events:?}");
    let ready = events.iter().position(|e| e["event"] == "ready").expect("ready");
    assert!(ready > fallback && events[ready]["engine"] == "python", "{events:?}");
    // the next load (after an unload) goes straight to Python (`events` drains: only the new load's events)
    service.stop();
    let events = ready_engine(&service);
    assert!(!events.iter().any(|e| e["event"] == "engine_fallback"), "{events:?}");
    assert_eq!(events.iter().rev().find(|e| e["event"] == "ready").unwrap()["engine"], "python");
    service.stop();
}

#[test]
fn a_rust_engine_that_only_stalls_gets_a_second_chance_then_python_takes_over() {
    // stdout closed but still running: killed (not its own exit), one strike, the load retry is the second
    let (_dir, service) = engines("exec 1>&-; sleep 30");
    service.configure(true);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = Vec::new();
    while !service.ready() {
        assert!(Instant::now() < deadline, "{seen:?}");
        seen.extend(service.events());
        std::thread::sleep(Duration::from_millis(10));
    }
    seen.extend(service.events());
    let strike = seen.iter().position(|e| e["event"] == "engine_strike").expect("a strike first");
    assert!(seen[strike]["reason"].as_str().unwrap().contains("killed"), "{seen:?}");
    let fallback = seen.iter().position(|e| e["event"] == "engine_fallback").expect("then the fallback");
    assert!(fallback > strike, "{seen:?}");
    assert_eq!(seen.iter().rev().find(|e| e["event"] == "ready").unwrap()["engine"], "python");
    service.stop();
}

#[test]
fn speaking_off_during_a_rust_load_is_not_the_engines_fault() {
    let (_dir, service) = engines("sleep 30");
    service.configure(true);
    std::thread::sleep(Duration::from_millis(100));
    service.configure(false);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!service.events().iter().any(|e| e["event"] == "engine_strike" || e["event"] == "engine_fallback"));
    service.configure(true);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!service.events().iter().any(|e| e["event"] == "engine_fallback"));
    service.stop();
}

#[test]
fn each_request_carries_the_playback_speed() {
    let (dir, service) = engines("echo '{\"event\":\"ready\"}'; cat >/dev/null");
    std::fs::remove_file(dir.path().join("rust").join(super::SIDECAR_PROCESS)).unwrap();
    // the Python fixture writes every request it reads to requests.jsonl
    std::fs::write(dir.path().join("sidecar.py"), format!("import json,sys\nprint(json.dumps({{'event':'ready'}}),flush=True)\nfor l in sys.stdin:\n open({:?},'a').write(l)\n r=json.loads(l)\n print(json.dumps({{'event':'error','id':r['id'],'reason':'fixture'}}),flush=True)\n", dir.path().join("requests.jsonl"))).unwrap();
    let pace = Arc::new(std::sync::atomic::AtomicU16::new(160));
    service.set_pace(pace);
    ready_engine(&service);
    let _ = service.stream("Hello there.", &|| false, &mut |_| Ok(()));
    let sent = std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap();
    let first: Value = serde_json::from_str(sent.lines().next().unwrap()).unwrap();
    assert_eq!(first["rate"], 1.6, "{sent}");
    service.stop();
}

#[test]
fn a_memory_refusal_or_cancel_is_not_a_reason_to_change_engines() {
    assert!(!super::python_takes_over("sidecar load: {\"event\":\"fatal\"}; Memory unload (exit 73)"));
    assert!(!super::python_takes_over("load cancelled"));
    assert!(!super::python_takes_over("sidecar load: {\"event\":\"memory_refusal\",\"reason\":\"swap growing\"}"));
    assert!(super::python_takes_over("sidecar load: channel is empty and sending half is closed"));
    assert!(super::python_takes_over("Drama 3 load deadline"));
    // a number with 73 in a worker's own line is not exit 73
    assert!(super::python_takes_over("Rust engine exited (exit status: 1): sidecar load: {\"event\":\"fatal\",\"load_s\":1.73,\"reason\":\"bad kernel\"}"));
    assert!(!super::memory_reason("sidecar load: {\"event\":\"ready\",\"load_s\":1.73}"));
    assert!(super::memory_reason("sidecar exited exit status: 73: no reason given"));
    assert!(!super::python_takes_over("Rust engine exited (exit status: 73): sidecar load: channel is empty and sending half is closed"));
    assert!(!super::python_takes_over("sidecar load: {\"event\":\"fatal\",\"reason\":\"Swap grew by 1.2 GB. Using the lighter voice\"}"));
    let (_dir, service) = engines("echo '{\"event\":\"fatal\",\"reason\":\"Need 55 GB available before loading\"}'; exit 73");
    service.configure(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.events().iter().any(|e| e["event"] == "memory_unload") {
        assert!(Instant::now() < deadline, "{:?}", service.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(!service.ready());
    assert!(!service.events().iter().any(|e| e["event"] == "engine_fallback"), "{:?}", service.events());
    service.stop();
}

/// Bug note 2026-11-03T22-38-44: a job's declared need over the 95 GB line makes Jarvis give way (never mid-line),
/// the island hears of it once, he stays out while the need stands, and he comes back a minute after memory is back,
/// between replies.
#[test]
fn over_the_memory_line_jarvis_gives_way_holds_and_returns_between_replies() {
    use crate::memory_line::{Gate, Sample, Source};
    struct Shared(Arc<Mutex<Sample>>);
    impl Source for Shared {
        fn sample(&mut self) -> Result<Sample> {
            Ok(self.0.lock().unwrap().clone())
        }
    }
    let gb = 1u64 << 30;
    let reading = Arc::new(Mutex::new(Sample { used: 60 * gb, pressure: 1, ..Default::default() }));
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let start = Instant::now();
    let clock = {
        let offset = offset.clone();
        move || start + *offset.lock().unwrap()
    };
    let later = |s: u64| *offset.lock().unwrap() += Duration::from_secs(s);
    let (_dir, s) = sidecar("import json,sys\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin: pass\n");
    s.set_memory_gate(Gate::new(Box::new(Shared(reading.clone()))).with_clock(Box::new(clock)));
    s.configure(true);
    settle(&s);
    assert!(s.ready(), "{:?}", s.events());
    s.configure(true);
    assert!(s.ready(), "60 GB in use: he stays");
    // The Arsenal restore declares 40 GB: 60 + 40 passes 95.
    {
        let mut r = reading.lock().unwrap();
        r.need = 40 * gb;
        r.why = vec!["Arsenal locomotive restore".into()];
    }
    later(2);
    // A line is in flight (it holds the worker): he is not cut mid-line, but no new line takes him.
    {
        let _line = s.process.lock().unwrap();
        s.configure(true);
        assert!(!s.ready() && s.ready.load(Ordering::Acquire) && s.pid.load(Ordering::Acquire) > 0);
        assert!(s.memory_held().is_some());
    }
    later(2);
    s.configure(true);
    assert!(!s.ready.load(Ordering::Acquire) && s.pid.load(Ordering::Acquire) == 0, "unloaded once the line ended");
    assert!(s.policy_waiting(), "a memory hold is no failure");
    assert_eq!(s.status().as_deref(), Some(NOTE_MEMORY));
    let events = s.events();
    let gave = events.iter().find(|v| v["event"] == "memory_give_way").expect("memory_give_way");
    assert_eq!(gave["need_bytes"], 40 * gb);
    assert_eq!(gave["needed_by"][0], "Arsenal locomotive restore");
    assert!(gave["reason"].as_str().unwrap().starts_with("Memory would pass 95 GB (60.0 GB in use plus 40.0 GB needed"));
    assert_eq!(events.iter().filter(|v| v["event"] == "memory_give_way").count(), 1);
    assert!(events.iter().any(|v| v["event"] == "memory_unload" && v["phase"] == "give_way"));
    // Phonon S says the lines; the island hears of it once.
    s.lighter_voice("First.", NOTICE_NOT_LOADED, "not loaded");
    s.lighter_voice("Second.", NOTICE_NOT_LOADED, "not loaded");
    let events = s.events();
    let switches: Vec<_> = events.iter().filter(|v| v["event"] == "voice_switch").collect();
    assert_eq!(switches.len(), 1, "{events:?}");
    assert_eq!(switches[0]["notice"], NOTICE_MEMORY);
    assert_eq!(switches[0]["cause"], "memory");
    assert_eq!(events.iter().filter(|v| v["event"] == "memory_line").count(), 1);
    // Held while the need stands, however long.
    for _ in 0..6 {
        later(30);
        s.configure(true);
        s.resume();
        assert!(!s.starting.load(Ordering::Acquire) && !s.ready());
    }
    // The job ends: 60 GB in use leaves room for him and the margin. Not before a minute has passed.
    {
        let mut r = reading.lock().unwrap();
        r.need = 0;
        r.why.clear();
    }
    later(2);
    s.configure(true);
    later(30);
    s.configure(true);
    assert!(!s.starting.load(Ordering::Acquire), "back for 30 s only");
    later(31);
    {
        let _line = s.speaking();
        s.configure(true);
        assert!(!s.starting.load(Ordering::Acquire), "never while a line is spoken");
    }
    s.configure(true);
    assert!(!s.starting.load(Ordering::Acquire) && !s.ready(), "a line just ended: the gap between replies is 3 s");
    *s.last_spoke.lock().unwrap() = Some(Instant::now() - Duration::from_secs(5));
    s.configure(true);
    assert!(!s.loading(), "a line meanwhile does not wait for his return");
    settle(&s);
    assert!(s.ready(), "{:?}", s.events());
    assert!(s.events().iter().any(|v| v["event"] == "memory_return"));
    assert!(s.memory_held().is_none());
    s.stop();
}

struct SharedReading(Arc<Mutex<crate::memory_line::Sample>>);
impl crate::memory_line::Source for SharedReading {
    fn sample(&mut self) -> Result<crate::memory_line::Sample> {
        Ok(self.0.lock().unwrap().clone())
    }
}
/// A gate on a shared reading, and a clock the test moves by whole seconds.
fn memory_gate(s: &Service, used_gib: u64, pressure: i64) -> (Arc<Mutex<crate::memory_line::Sample>>, impl Fn(u64)) {
    let reading = Arc::new(Mutex::new(crate::memory_line::Sample { used: used_gib << 30, pressure, ..Default::default() }));
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let start = Instant::now();
    let clock = {
        let offset = offset.clone();
        move || start + *offset.lock().unwrap()
    };
    s.set_memory_gate(crate::memory_line::Gate::new(Box::new(SharedReading(reading.clone()))).with_clock(Box::new(clock)));
    (reading, move |secs| *offset.lock().unwrap() += Duration::from_secs(secs))
}
const SLOW_LOAD: &str = "import json,sys,time\ntime.sleep(.6)\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin: pass\n";

/// Review 4 Nov: judged "not loaded" during his own load, Jarvis's rising use failed the room check partway, so he
/// was held, then gave way once ready, returned a minute later and looped; lines waited for each doomed load. His load
/// is his: memory it takes never holds him, and only passing the line itself makes him give way.
#[test]
fn memory_rising_during_his_own_load_never_holds_him() {
    let (_dir, s) = sidecar(SLOW_LOAD);
    let (reading, later) = memory_gate(&s, 60, 1);
    s.configure(true);
    assert!(s.starting.load(Ordering::Acquire));
    // 20 GiB of him are in: 80 GiB in use, too little room for another 24 GiB, but it is his.
    reading.lock().unwrap().used = 80 << 30;
    later(2);
    s.configure(true);
    assert!(s.memory_held().is_none(), "{:?}", s.events());
    assert!(s.loading(), "a line waits for him as usual");
    settle(&s);
    later(2);
    s.configure(true);
    assert!(s.ready(), "{:?}", s.events());
    s.stop();
}
#[test]
fn passing_the_line_during_his_load_gives_way_and_no_line_waits_for_it() {
    let (_dir, s) = sidecar(SLOW_LOAD);
    let (reading, later) = memory_gate(&s, 60, 1);
    s.configure(true);
    assert!(s.starting.load(Ordering::Acquire));
    {
        let mut r = reading.lock().unwrap();
        r.used = 90 << 30;
        r.need = 10 << 30;
    }
    later(2);
    s.configure(true);
    assert!(s.memory_held().is_some());
    assert!(!s.loading(), "a line goes to Phonon S rather than wait for a load that ends in an unload");
    settle(&s);
    s.configure(true);
    assert!(!s.ready.load(Ordering::Acquire) && s.pid.load(Ordering::Acquire) == 0, "unloaded once his load ended");
    assert!(!s.starting.load(Ordering::Acquire));
}

/// Review 4 Nov: a give-way during his load could not take the worker, so the load ran to the end (~27 GB for up to
/// 45 s) while memory was over the line. The load stops within its 20 ms poll, the half-loaded worker is killed, and
/// it counts as a give-way: no failure, no retry, no strike, no reload while held.
#[test]
fn a_give_way_during_his_load_stops_it_and_kills_the_half_loaded_worker() {
    let (_dir, s) = sidecar("import json,sys,time\ntime.sleep(30)\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin: pass\n");
    let (reading, later) = memory_gate(&s, 60, 1);
    s.configure(true);
    let deadline = Instant::now() + Duration::from_secs(5);
    while s.pid.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < deadline, "{:?}", s.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid = s.pid.load(Ordering::Acquire);
    reading.lock().unwrap().need = 40 << 30;
    later(2);
    let gave = Instant::now();
    s.configure(true);
    settle(&s);
    assert!(gave.elapsed() < Duration::from_secs(2), "the load stopped in {:?}, not after its 30 s", gave.elapsed());
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < deadline, "the half-loaded worker {pid} is still alive");
        std::thread::sleep(Duration::from_millis(10));
    }
    let events = s.events();
    assert!(!events.iter().any(|v| v["event"] == "ready"), "{events:?}");
    let unload = events.iter().find(|v| v["event"] == "memory_unload" && v["cancelled"] == "load").expect("cancelled load");
    assert!(unload["reason"].as_str().unwrap().starts_with("Memory would pass 95 GB"), "{unload}");
    assert!(!events.iter().any(|v| ["failure", "load_retry", "engine_strike", "memory_wait"].contains(&v["event"].as_str().unwrap_or(""))), "{events:?}");
    assert!(s.failures.lock().unwrap().is_empty());
    assert!(!s.giving_way.load(Ordering::Acquire) && !s.ready.load(Ordering::Acquire) && s.pid.load(Ordering::Acquire) == 0);
    assert!(s.memory_held().is_some());
    assert_eq!(s.status().as_deref(), Some(NOTE_MEMORY));
    later(2);
    s.configure(true);
    s.resume();
    assert!(!s.starting.load(Ordering::Acquire), "held: nothing reloads him");
    s.stop();
}

/// Review 4 Nov: a give-way during the half-second pause before a load retry was undone by the retry itself, which
/// ignored the hold. The retry is skipped; the worker never starts a second time.
#[test]
fn a_hold_during_the_retry_pause_stops_the_retry() {
    let (dir, s) = sidecar("import json,os,sys\nm=os.path.join(os.path.dirname(os.path.abspath(__file__)),'runs')\nopen(m,'a').write('x')\nif len(open(m).read())==1:\n print(json.dumps({'event':'fatal','reason':'Too many open files (os error 24)'}),flush=True);sys.exit(1)\nprint(json.dumps({'event':'ready'}),flush=True)\nsys.stdin.read()\n");
    let (reading, later) = memory_gate(&s, 60, 1);
    s.configure(true);
    let mut seen = vec![];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.iter().any(|v: &Value| v["event"] == "load_retry") {
        assert!(Instant::now() < deadline, "{seen:?}");
        seen.extend(s.events());
        std::thread::sleep(Duration::from_millis(5));
    }
    reading.lock().unwrap().need = 40 << 30;
    later(2);
    s.configure(true);
    settle(&s);
    seen.extend(s.events());
    assert_eq!(std::fs::read_to_string(dir.path().join("runs")).unwrap(), "x", "no second worker: {seen:?}");
    assert!(!s.ready() && s.memory_held().is_some(), "{seen:?}");
    assert!(seen.iter().any(|v| v["event"] == "memory_unload" && v["cancelled"] == "load"), "{seen:?}");
    assert!(s.failures.lock().unwrap().is_empty(), "{seen:?}");
    s.stop();
}

/// Review 4 Nov: the memory notice was re-armed at every return, so a return the sidecar refused (exit 73) showed
/// "Jarvis off for memory · Phonon S" again each cycle. The user hears of a hold once; again only after Jarvis was back.
#[test]
fn the_memory_notice_repeats_only_after_jarvis_was_actually_back() {
    let (_dir, s) = sidecar("import json,os,sys\nm=os.path.join(os.path.dirname(os.path.abspath(__file__)),'runs')\nopen(m,'a').write('x')\nif len(open(m).read())<=2:\n print(json.dumps({'event':'fatal','reason':'Not enough free memory for Jarvis right now (needs 31.1 GB, 20.0 GB free)'}),flush=True);os._exit(73)\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin: pass\n");
    let (reading, later) = memory_gate(&s, 60, 1);
    let notices = |s: &Service| s.events().iter().filter(|v| v["event"] == "voice_switch" && v["notice"] == NOTICE_MEMORY).count();
    let returned = |s: &Arc<Service>, secs: u64| {
        later(1);
        s.configure(true);
        later(secs);
        s.configure(true);
        settle(s);
    };
    // Refused at Speaking on: held, and the user hears of it once.
    s.configure(true);
    settle(&s);
    assert!(s.memory_held().is_some(), "{:?}", s.events());
    s.lighter_voice("One.", NOTICE_NOT_LOADED, "not loaded");
    assert_eq!(notices(&s), 1);
    // A minute later he returns, and the sidecar refuses again: the same hold to the user, now two minutes long.
    returned(&s, 60);
    assert!(s.memory_held().is_some() && !s.ready(), "{:?}", s.events());
    s.lighter_voice("Two.", NOTICE_NOT_LOADED, "not loaded");
    assert_eq!(notices(&s), 0, "no second notice for a refused return");
    returned(&s, 120);
    assert!(s.ready(), "{:?}", s.events());
    // Back, then over the line again: a new hold, a new notice.
    reading.lock().unwrap().need = 40 << 30;
    later(2);
    s.configure(true);
    s.lighter_voice("Three.", NOTICE_NOT_LOADED, "not loaded");
    assert_eq!(notices(&s), 1);
    s.stop();
}

/// The worker's own memory exit (native WARN in its guard, exit 73) goes through `failed()` into the same hold: no
/// 30 s retry into the shortage; he returns only once memory has been back for a minute.
#[test]
fn a_worker_memory_exit_waits_for_memory_to_be_back_not_30_s() {
    let (_dir, s) = sidecar("import json,os\nprint(json.dumps({'event':'fatal','reason':'System memory pressure. Using the lighter voice.'}),flush=True)\nos._exit(73)\n");
    let (_reading, later) = memory_gate(&s, 40, 1);
    s.configure(true);
    assert!(s.starting.load(Ordering::Acquire), "40 GiB in use: he loads");
    settle(&s);
    assert!(s.memory_held().is_some_and(|why| why.contains("pressure")), "{:?} {:?}", s.memory_held(), s.events());
    assert!(s.failures.lock().unwrap().is_empty(), "a memory exit is no crash");
    *s.memory_wait.lock().unwrap() = Some(Instant::now() - Duration::from_secs(31));
    s.configure(true);
    assert!(!s.starting.load(Ordering::Acquire), "the 30 s wait is not what holds him");
    later(30);
    s.configure(true);
    assert!(!s.starting.load(Ordering::Acquire));
    assert_eq!(s.status().as_deref(), Some(NOTE_MEMORY));
    // Speaking off and on: the note comes back with the hold.
    s.stop();
    s.configure(true);
    assert_eq!(s.status().as_deref(), Some(NOTE_MEMORY));
    later(31);
    s.configure(true);
    assert!(s.events().iter().any(|v| v["event"] == "memory_return"), "a minute back: he returns");
    settle(&s);
    assert!(s.memory_held().is_some(), "the worker exited for memory again: held again");
}
