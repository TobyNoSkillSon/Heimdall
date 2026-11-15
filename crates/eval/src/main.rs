mod screen_bench_run;
use anyhow::{bail, Context, Result};
use heimdall_eval::{
    load_manifest, pauses, read_recording, word_edits, Confusion, Pause, Recording,
};
use listen::{AudioChunk, ListenEngine, ListenEvent, Word};
use screen::{PhraseMatcher, TimedWord};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant},
};
fn main() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut manifest = root.join("lab/eval/data/sample-300.jsonl");
    let mut models = root.join("models");
    let mut out = root.join("lab/eval/data/replay-300");
    let (mut labels_only, mut realtime, mut allow_full) = (false, false, false);
    let mut args = std::env::args().skip(1);
    if std::env::args().nth(1).as_deref() == Some("screen-phrase-bench") {
        return screen_bench_run::run(&root, args.skip(1));
    }
    while let Some(a) = args.next() {
        match a.as_str() {
            "--manifest" => manifest = args.next().context("Missing manifest")?.into(),
            "--models" => models = args.next().context("Missing models")?.into(),
            "--out" => out = args.next().context("Missing output")?.into(),
            "--labels-only" => labels_only = true,
            "--realtime" => realtime = true,
            "--allow-full-corpus" => allow_full = true,
            _ => bail!("Unknown argument: {a}"),
        }
    }
    let rows = load_manifest(&manifest)?;
    anyhow::ensure!(!rows.is_empty(), "Empty manifest");
    if !labels_only && rows.len() > 300 && !allow_full {
        bail!("Replay capped at 300. Full corpus needs explicit authority and --allow-full-corpus; daytime runs use low-priority chunks.");
    }
    let out = heimdall_eval::prepare_output(&root, &out)?;
    if labels_only {
        let mut vad = listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?;
        let mut output = BufWriter::new(File::create(out.join("labels.jsonl"))?);
        let (mut count, mut failed) = (0, 0);
        for (i, mut row) in rows.into_iter().enumerate() {
            let r: Recording = serde_json::from_value(row.clone())?;
            match read_recording(&r).and_then(|a| pauses(&mut vad, &a)) {
                Ok(p) => {
                    count += p.len();
                    row["labels"]["internal_pauses"] = serde_json::to_value(p)?;
                    row["labels"]["vad_model"] =
                        json!("silero_vad.onnx; start=0.5 continue=0.35; 32ms blocks");
                }
                Err(e) => {
                    failed += 1;
                    row["label_error"] = json!(format!("{e:#}"));
                }
            }
            writeln!(output, "{}", serde_json::to_string(&row)?)?;
            if (i + 1) % 100 == 0 {
                output.flush()?;
                eprintln!(
                    "labelled={} internal_pauses={} errors={}",
                    i + 1,
                    count,
                    failed
                );
            }
        }
        output.flush()?;
        eprintln!(
            "labels_complete internal_pauses={} errors={}",
            count, failed
        );
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    bail!("Parakeet Ultra requires macOS");
    #[cfg(target_os = "macos")]
    {
        let mut vad = listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?;
        let asr = listen::asr::Parakeet::load(&models.join("parakeet-ultra"))?;
        let mut engine = ListenEngine::load(&models, asr)?;
        let mut screen = ScreenPhrases::default();
        let mut results = BufWriter::new(File::create(out.join("recordings.jsonl"))?);
        let mut dry = BufWriter::new(File::create(out.join("dry-run.jsonl"))?);
        let (
            mut edits,
            mut reference_words,
            mut premature_files,
            mut premature_cuts,
            mut internal_count,
        ) = (0, 0, 0, 0, 0);
        let mut delays = Vec::new();
        let mut wer_excluded_no_reference = 0;
        let (mut successful, mut failures, mut missing_endpoint) = (0, 0, 0);
        let (mut audio_seconds, mut compute_seconds) = (0.0, 0.0);
        let mut screenshot = Confusion::default();
        for (index, row) in rows.iter().enumerate() {
            let r: Recording = serde_json::from_value(row.clone())?;
            screen.reset(&r.id);
            let run = (|| -> Result<Value> {
                let audio = read_recording(&r)?;
                let p: Vec<Pause> = if row["labels"]["vad_model"].is_string() {
                    serde_json::from_value(row["labels"]["internal_pauses"].clone())?
                } else {
                    pauses(&mut vad, &audio)?
                };
                let start = Instant::now();
                let cpu_start = cpu_seconds();
                let wall_start = wall_seconds();
                let mut hypothesis = Vec::new();
                let mut segment_prefix = String::new();
                let mut quiet_start = None;
                let mut last_voice_end_ms = 0.0;
                engine.set_source_policy(&r.id, listen::PartialPolicy::Responsive)?;
                let mut finals = Vec::new();
                let mut last_screen_index = 0;
                let padded_len = audio.len() + 48000;
                let mut cursor = 0;
                // 20ms arrivals + 3s terminal silence. No EOF timing oracle.
                while cursor < padded_len {
                    let end = (cursor + 320).min(padded_len);
                    let mut chunk = vec![0.0; end - cursor];
                    let actual = audio.len().saturating_sub(cursor).min(chunk.len());
                    if actual > 0 {
                        chunk[..actual].copy_from_slice(&audio[cursor..cursor + actual]);
                    }
                    if realtime {
                        let target = Duration::from_secs_f64(cursor as f64 / 16000.0);
                        if target > start.elapsed() {
                            std::thread::sleep(target - start.elapsed());
                        }
                    }
                    for event in engine.push(AudioChunk {
                        source: &r.id,
                        generation: 0,
                        host_time_ns: cursor as u64 * 62500,
                        samples: &chunk,
                    })? {
                        match &event {
                            ListenEvent::SpeechStart { .. } => quiet_start = None,
                            ListenEvent::SpeechEnd { host_time_ns, .. } => {
                                quiet_start = Some(*host_time_ns);
                                last_voice_end_ms = *host_time_ns as f64 / 1e6;
                            }
                            _ => {}
                        }
                        if let ListenEvent::TranscriptSegment { text, .. } = &event {
                            if !segment_prefix.is_empty() {
                                segment_prefix.push(' ');
                            }
                            segment_prefix.push_str(text);
                        }
                        if let ListenEvent::PartialTranscript { words, .. } = &event {
                            screen.observe(&r.id, words)?;
                        }
                        if let ListenEvent::FinalUtterance {
                            text,
                            words,
                            start_ns,
                            end_ns,
                            end_of_turn_probability,
                            forced,
                            ..
                        } = event
                        {
                            let text = if segment_prefix.is_empty() {
                                text
                            } else {
                                format!("{} {}", std::mem::take(&mut segment_prefix), text)
                            };
                            hypothesis.push(text.clone());
                            screen.observe(&r.id, &words)?;
                            finals.push(json!({"text":text,"words":words,"start_ns":start_ns,"end_ns":end_ns,
                                "observed_ms":end as f64/16.0,"probability":end_of_turn_probability,"forced":forced,"eof":false}));
                            let last = finals.len() - 1;
                            finals[last]["silence_ms"] =
                                json!(continuous_silence_ms(end as u64 * 62500, quiet_start,));
                            finals[last]["fixed_screen_phrases"] =
                                json!(&screen.phrases[last_screen_index..]);
                            last_screen_index = screen.phrases.len();
                        }
                    }
                    cursor = end;
                }
                for event in engine.remove_source(&r.id)? {
                    if let ListenEvent::FinalUtterance {
                        text,
                        words,
                        start_ns,
                        end_ns,
                        end_of_turn_probability,
                        forced,
                        ..
                    } = event
                    {
                        let text = if segment_prefix.is_empty() {
                            text
                        } else {
                            format!("{} {}", std::mem::take(&mut segment_prefix), text)
                        };
                        hypothesis.push(text.clone());
                        finals.push(json!({"text":text,"words":words,"start_ns":start_ns,"end_ns":end_ns,
                        "observed_ms":padded_len as f64/16.0,"probability":end_of_turn_probability,"forced":forced,"eof":true}));
                        let last = finals.len() - 1;
                        finals[last]["silence_ms"] = json!(continuous_silence_ms(
                            padded_len as u64 * 62500,
                            quiet_start
                        ));
                        finals[last]["fixed_screen_phrases"] =
                            json!(&screen.phrases[last_screen_index..]);
                        last_screen_index = screen.phrases.len();
                    }
                }
                let elapsed = start.elapsed().as_secs_f64();
                let hypothesis = hypothesis.join(" ");
                let (e, w) = word_edits(&r.vella_text, &hypothesis);
                let cuts = finals
                    .iter()
                    .filter(|f| {
                        let at = f["observed_ms"].as_f64().unwrap();
                        p.iter().any(|p| at >= p.start_ms && at <= p.end_ms + 32.0)
                    })
                    .count();
                let delay = finals
                    .iter()
                    .next_back()
                    .filter(|f| {
                        f["eof"] == false
                            && !p.iter().any(|p| {
                                let at = f["observed_ms"].as_f64().unwrap();
                                at >= p.start_ms && at <= p.end_ms + 32.0
                            })
                    })
                    .map(|f| (f["observed_ms"].as_f64().unwrap() - r.duration * 1000.0).max(0.0));
                if w > 0 {
                    edits += e;
                    reference_words += w;
                } else {
                    wer_excluded_no_reference += 1;
                }
                premature_files += usize::from(cuts > 0);
                premature_cuts += cuts;
                internal_count += p.len();
                if let Some(d) = delay {
                    delays.push(d);
                } else {
                    missing_endpoint += 1;
                }
                audio_seconds += r.duration;
                compute_seconds += elapsed;
                successful += 1;
                let any_screenshot = !screen.phrases.is_empty();
                for f in &finals {
                    let phrase = f["fixed_screen_phrases"].as_array().is_some_and(|p| !p.is_empty());
                    writeln!(
                        dry,
                        "{}",
                        json!({"mode":"DRY-RUN","recording":r.id,"observed_ms":f["observed_ms"],
                        "would_append":if phrase {
                            json!(format!("{} [DRY-RUN: would capture screen; historical screen frames unavailable]",f["text"].as_str().unwrap_or("")))
                        }else{f["text"].clone()},
                        "would_attach_screenshot":phrase,"fixed_screen_phrases":f["fixed_screen_phrases"],"cloud_calls":0})
                    )?;
                }
                screenshot.add(r.has_image, Some(any_screenshot));
                Ok(
                    json!({"recording":r.id,"audio_seconds":r.duration,"vella_reference":r.vella_text,"reference_kind":"model_output_not_ground_truth",
                    "last_voice_end_ms":last_voice_end_ms,"hypothesis":hypothesis,"edits":e,"reference_words":w,"reference_available":w>0,"internal_pauses":p,"finals":finals,"premature_cuts":cuts,"added_delay_ms":delay,
                    "process_cpu_seconds":cpu_seconds()-cpu_start,"replay_wall_start":wall_start,"replay_wall_end":wall_seconds(),"paced_wall_seconds":elapsed,
                    "engine_seconds":elapsed,"fixed_screen_phrases":screen.phrases,"matched":r.matched_message.is_some()}),
                )
            })();
            match run {
                Ok(value) => writeln!(results, "{value}")?,
                Err(error) => {
                    failures += 1;
                    let _ = engine.remove_source(&r.id);
                    writeln!(
                        results,
                        "{}",
                        json!({"recording":r.id,"error":format!("{error:#}")})
                    )?;
                }
            }
            results.flush()?;
            dry.flush()?;
            if (index + 1) % 10 == 0 {
                eprintln!(
                    "replayed={}/{} successful={} errors={}",
                    index + 1,
                    rows.len(),
                    successful,
                    failures
                );
            }
            delays.sort_by(f64::total_cmp);
            let percentile = |f: f64| -> Option<f64> {
                if delays.is_empty() {
                    None
                } else {
                    Some(delays[((delays.len() - 1) as f64 * f).round() as usize])
                }
            };
            let metrics = json!({"attempted":index+1,"sample_size":rows.len(),"successful":successful,"failed":failures,"complete":index+1==rows.len(),
                "wer":heimdall_eval::ratio(edits,reference_words),"word_edits":edits,"reference_words":reference_words,"wer_excluded_no_reference":wer_excluded_no_reference,"reference":"Vella ASR output, not ground truth",
                "sample_weighting":"unweighted stratified sample, not population estimate","premature_cut_recording_rate":heimdall_eval::ratio(premature_files,successful),
                "premature_recordings":premature_files,"premature_cuts":premature_cuts,"internal_pauses":internal_count,
                "added_delay_mean_ms":if delays.is_empty(){None}else{Some(delays.iter().sum::<f64>()/delays.len()as f64)},"added_delay_p50_ms":percentile(0.5),
                "added_delay_p95_ms":percentile(0.95),"missing_natural_endpoint":missing_endpoint,
                "delay_definition":"emission audio-clock minus recording end floored at zero; 3s terminal silence; EOF flush excluded",
                "screenshot":{"confusion":screenshot,"precision":screenshot.precision(),"recall":screenshot.recall(),"source":"fixed screen phrases only"},
                "audio_seconds":audio_seconds,"pipeline_wall_seconds":compute_seconds,"realtime":realtime,
                "rtf":if audio_seconds>0.0{Some(compute_seconds/audio_seconds)}else{None},"cloud_calls":0});
            let temp = out.join("metrics.json.tmp");
            fs::write(&temp, serde_json::to_string_pretty(&metrics)? + "\n")?;
            fs::rename(temp, out.join("metrics.json"))?;
        }
        println!("{}", fs::read_to_string(out.join("metrics.json"))?);
    }
    Ok(())
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // Safety: getrusage initializes a valid out pointer; checked before reading.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return f64::NAN;
    }
    let u = unsafe { usage.assume_init() };
    u.ru_utime.tv_sec as f64
        + u.ru_utime.tv_usec as f64 / 1e6
        + u.ru_stime.tv_sec as f64
        + u.ru_stime.tv_usec as f64 / 1e6
}
fn wall_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
/// The screen `PhraseMatcher`'s hits on the replayed words ("look at this", "take a screenshot"); it logs them and
/// takes no frames. Offline-only: no capture, screen permission, agent session or network API.
#[derive(Default)]
struct ScreenPhrases {
    phrases: Vec<Value>,
    matcher: PhraseMatcher,
}
impl ScreenPhrases {
    fn reset(&mut self, id: &str) {
        self.phrases.clear();
        self.matcher.reset_source(id);
    }
    fn observe(&mut self, id: &str, words: &[Word]) -> Result<()> {
        let timed: Vec<_> = words.iter().map(|w| TimedWord { text: w.text.clone(), start_ns: w.start_ns, end_ns: w.end_ns }).collect();
        for p in self.matcher.observe(id, &timed) {
            self.phrases.push(serde_json::to_value(p)?);
        }
        Ok(())
    }
}

// A forced line flush is not a silence event. Only SpeechEnd starts this clock;
// SpeechStart clears it, including when an utterance rolls while still speaking.
fn continuous_silence_ms(now_ns: u64, quiet_start: Option<u64>) -> u64 {
    quiet_start.map_or(0, |start| now_ns.saturating_sub(start) / 1_000_000)
}
#[cfg(test)]
mod timing_tests {
    use super::*;
    #[test]
    fn flush_without_measured_quiet_cannot_authorize_send() {
        assert_eq!(continuous_silence_ms(90_000_000_000, None), 0);
        assert_eq!(
            continuous_silence_ms(90_000_000_000, Some(89_500_000_000)),
            500
        );
        assert_eq!(
            continuous_silence_ms(90_000_000_000, Some(87_500_000_000)),
            2500
        );
    }
}
