//! `heimdall-eval screen-phrase-bench`: synthetic speech with known phrase onsets through the real
//! listen engine (Silero + Smart Turn + Parakeet, Metal like the app), the production PhraseMatcher,
//! and the production screen hold/attach path over a synthetic screen recorded through the real
//! archive with the hardware HEVC encoder. See lab/screen-phrase-bench/REPORT.md.
use anyhow::{bail, ensure, Context, Result};
use heimdall_eval::screen_bench::*;
use listen::{AudioChunk, ListenEngine, ListenEvent, Word};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

/// The app's decision constants (crates/app/src/pipeline/mod.rs), mirrored for the decision window.
const SIDE_MAX_NS: u64 = 15_000_000_000;
const SIDE_AFTER_NS: u64 = 200_000_000;
const PHRASE_FALLBACK_NS: u64 = 1_000_000_000;
/// Screen timeline origin (host ns) and the first utterance block.
const SCREEN_START_NS: u64 = 10_000_000_000;
const LEAD_SILENCE: usize = RATE; // 1 s before speech
const TAIL_SILENCE: usize = 3 * RATE; // > 1.5 s the user policy pause and 2.5 s forced end
const SPEECH_RMS: f64 = 0.06; // ≈ -24 dBFS over speech

#[derive(Deserialize, Clone)]
struct Item {
    #[allow(dead_code)] // documents the inventory row
    id: String,
    text: String,
    #[serde(default)]
    kind: String,
}
#[derive(Deserialize)]
struct LangInv {
    leads: Vec<Item>,
    phrases: Vec<Item>,
    tails: Vec<Item>,
    not_in_table: Vec<Item>,
    negatives: Vec<Item>,
}
#[derive(Deserialize)]
struct Voice {
    engine: String,
    voice: String,
}
#[derive(Deserialize)]
struct Inventory {
    en: LangInv,
    pl: LangInv,
    voices: HashMap<String, Vec<Voice>>,
}
#[derive(Deserialize)]
struct TtsRow {
    lang: String,
    engine: String,
    voice: String,
    rate: String,
    text: String,
    path: String,
}

#[derive(Clone)]
struct Case {
    id: String,
    lang: String,
    engine: String,
    voice: String,
    rate: String,
    /// start | start_whole | lead_word | frame | middle | end | negative | not_in_table
    position: String,
    phrase: Option<String>,
    text: String,
    noise: String,
    snr_db: Option<f64>,
    /// Production matcher on the reference text: does it fire, and at the phrase's first word?
    expected_fire: bool,
    reference_fires: bool,
    audio: Vec<f32>,
    /// Sample index of the phrase's first speech sample (positives).
    onset: Option<usize>,
    speech: (usize, usize),
    start_ns: u64,
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn timed(text: &str) -> Vec<screen::TimedWord> {
    text.split_whitespace()
        .enumerate()
        .map(|(i, w)| screen::TimedWord {
            text: w.into(),
            start_ns: 1_000_000_000 + i as u64 * 300_000_000,
            end_ns: 1_000_000_000 + i as u64 * 300_000_000 + 250_000_000,
        })
        .collect()
}
/// Matches of the production screen table on reference text (as Parakeet would print it).
fn reference_matches(text: &str) -> Vec<(usize, String)> {
    let words = timed(text);
    screen::PhraseMatcher::default()
        .observe_utterance("ref", "ref", &words, true)
        .into_iter()
        .map(|m| (((m.start_ns - 1_000_000_000) / 300_000_000) as usize, m.phrase))
        .collect()
}

fn read_wav(path: &str) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {path}"))?;
    let s = r.spec();
    ensure!(s.sample_rate == RATE as u32 && s.channels == 1, "{path}: need 16 kHz mono");
    Ok(match s.sample_format {
        hound::SampleFormat::Int => r.samples::<i32>().map(|v| v.map(|v| v as f32 / (1 << (s.bits_per_sample - 1)) as f32)).collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    })
}

struct Tts {
    rows: HashMap<(String, String, String, String, String), String>,
    cache: HashMap<String, Trimmed>,
    /// Off for cases outside the shard: a stand-in segment, nothing read (the case plan and its random draws are
    /// unchanged, so the shard's cases are the same ones as in a full build).
    load: bool,
    /// `--list-tts`: record every render the shard reads instead of reading it.
    listed: Option<std::collections::BTreeSet<String>>,
}
impl Tts {
    fn stand_in() -> Trimmed {
        trim(&[vec![0.0; 400], vec![0.5; 1600], vec![0.0; 400]].concat()).unwrap()
    }
    fn wav(&mut self, path: &str) -> Vec<f32> {
        if let Some(listed) = &mut self.listed {
            listed.insert(path.into());
            return vec![];
        }
        read_wav(path).unwrap_or_default()
    }
    fn get(&mut self, lang: &str, v: &Voice, rate: &str, text: &str) -> Result<Trimmed> {
        let key = (lang.into(), v.engine.clone(), v.voice.clone(), rate.into(), text.into());
        let path = self.rows.get(&key).with_context(|| format!("no TTS for {key:?}"))?.clone();
        if !self.load {
            return Ok(Self::stand_in());
        }
        if let Some(listed) = &mut self.listed {
            listed.insert(path);
            return Ok(Self::stand_in());
        }
        if let Some(t) = self.cache.get(&path) {
            return Ok(t.clone());
        }
        let t = trim(&read_wav(&path)?).with_context(|| format!("silent render {path}"))?;
        self.cache.insert(path, t.clone());
        Ok(t)
    }
}

struct Plan {
    position: &'static str,
    lead: Option<Item>,
    phrase: Option<Item>,
    tail: Option<Item>,
    whole: Option<String>,
}

fn plans(inv: &LangInv, rng: &mut Rng, combo: usize, lang: &str) -> Vec<Plan> {
    let words: Vec<_> = inv.leads.iter().filter(|l| l.kind == "word").cloned().collect();
    let frames: Vec<_> = inv.leads.iter().filter(|l| l.kind == "frame").cloned().collect();
    let sentences: Vec<_> = inv.leads.iter().filter(|l| l.kind == "sentence").cloned().collect();
    let mut kinds = vec!["start", "start_whole", "lead_word", "middle", "end", "end"];
    kinds.push(if frames.is_empty() { "lead_word" } else { "frame" });
    let mut out = vec![];
    for (k, kind) in kinds.into_iter().enumerate() {
        // Rotate phrases across combos so each voice/rate covers all of them.
        let phrase = inv.phrases[(combo * 7 + k) % inv.phrases.len()].clone();
        let tail = Some(rng.pick(&inv.tails).clone());
        let p = match kind {
            "start" => Plan { position: "start", lead: None, phrase: Some(phrase), tail, whole: None },
            "start_whole" => {
                let t = tail.unwrap();
                Plan { position: "start_whole", lead: None, whole: Some(capitalize(&phrase.text) + " " + &t.text), phrase: Some(phrase), tail: None }
            }
            "lead_word" => Plan { position: "lead_word", lead: Some(rng.pick(&words).clone()), phrase: Some(phrase), tail, whole: None },
            "frame" => Plan { position: "frame", lead: Some(rng.pick(&frames).clone()), phrase: Some(phrase), tail, whole: None },
            "middle" => Plan { position: "middle", lead: Some(rng.pick(&sentences).clone()), phrase: Some(phrase), tail, whole: None },
            _ => {
                let lead = if k % 2 == 0 { rng.pick(&words) } else { rng.pick(&sentences) };
                Plan { position: "end", lead: Some(lead.clone()), phrase: Some(phrase), tail: None, whole: None }
            }
        };
        out.push(p);
    }
    out.push(Plan { position: "negative", lead: None, phrase: None, tail: None, whole: Some(inv.negatives[combo % inv.negatives.len()].text.clone()) });
    if combo % 3 == 0 {
        let n = &inv.not_in_table[(combo / 3) % inv.not_in_table.len()];
        out.push(Plan { position: "not_in_table", lead: None, phrase: None, tail: None, whole: Some(n.text.clone()) });
    }
    let _ = lang;
    out
}

fn gap(rng: &mut Rng, lead: &Item) -> usize {
    let ms = match lead.kind.as_str() {
        "frame" => rng.range(30, 110),
        "sentence" => rng.range(250, 550),
        _ if lead.text.ends_with(',') => rng.range(150, 320),
        _ => rng.range(60, 180),
    };
    ms * RATE / 1000
}

const NOISE10: [&str; 4] = ["white", "pink", "babble", "music"];
const NOISE_OTHER: [(&str, f64); 6] = [("white", 20.0), ("babble", 20.0), ("pink", 5.0), ("babble", 5.0), ("music", 5.0), ("music", 20.0)];

/// Every case of the benchmark in a fixed order; only those whose index passes `keep` are built and returned (shards
/// read only their own renders). With `listed`, nothing is read: the renders the kept cases need are recorded there.
fn build_cases(root: &Path, tts_dir: &Path, seed: u64, only: Option<&str>, keep: &dyn Fn(usize) -> bool,
    listed: Option<&mut std::collections::BTreeSet<String>>) -> Result<Vec<Case>> {
    let inv: Inventory = serde_json::from_slice(&fs::read(root.join("lab/screen-phrase-bench/inventory.json"))?)?;
    let rows: Vec<TtsRow> = serde_json::from_slice(&fs::read(tts_dir.join("manifest.json")).context("TTS manifest: run gen_tts.py first")?)?;
    // Babble talkers say only phrase-free texts (lead sentences, long tails), so noise never carries a command.
    let mut babble_pool: HashMap<String, Vec<String>> = HashMap::new();
    for (lang, li) in [("en", &inv.en), ("pl", &inv.pl)] {
        let texts: Vec<&str> = li.leads.iter().filter(|l| l.kind == "sentence").chain(li.tails.iter().filter(|t| t.text.split_whitespace().count() >= 3)).map(|i| i.text.as_str()).collect();
        for r in rows.iter().filter(|r| r.lang == lang && texts.contains(&r.text.as_str())) {
            babble_pool.entry(lang.into()).or_default().push(r.path.clone());
        }
    }
    let mut tts = Tts {
        rows: rows.into_iter().map(|r| ((r.lang, r.engine, r.voice, r.rate, r.text), r.path)).collect(),
        cache: HashMap::new(),
        load: true,
        listed: listed.is_some().then(Default::default),
    };
    let mut cases = vec![];
    let mut index = 0usize;
    let mut base = 0usize;
    for lang in ["en", "pl"] {
        if only.is_some_and(|o| o != lang) {
            continue;
        }
        let li = if lang == "en" { &inv.en } else { &inv.pl };
        let mut combo = 0;
        for v in &inv.voices[lang] {
            for rate in ["slow", "normal", "fast"] {
                let mut rng = Rng(seed ^ (combo as u64 + 1).wrapping_mul(0x51_7C_C1_B7_27_22_0A_95) ^ if lang == "pl" { 0xABCD } else { 0 });
                for plan in plans(li, &mut rng, combo, lang) {
                    tts.load = (index..index + 3).any(keep);
                    let (spliced, text, phrase_word) = if let Some(w) = &plan.whole {
                        let t = tts.get(lang, v, rate, w)?;
                        (splice(&[t], &[], 0), w.clone(), 0)
                    } else {
                        let phrase = plan.phrase.as_ref().unwrap();
                        let mut parts = vec![];
                        let mut gaps = vec![];
                        let mut words = vec![];
                        let mut target = 0;
                        if let Some(l) = &plan.lead {
                            parts.push(tts.get(lang, v, rate, &l.text)?);
                            gaps.push(gap(&mut rng, l));
                            words.push(l.text.clone());
                            target = 1;
                        }
                        let shown = if plan.lead.is_none() || plan.lead.as_ref().unwrap().kind == "sentence" { capitalize(&phrase.text) } else { phrase.text.clone() };
                        parts.push(tts.get(lang, v, rate, &phrase.text)?);
                        if let Some(t) = &plan.tail {
                            gaps.push(rng.range(40, 150) * RATE / 1000);
                            parts.push(tts.get(lang, v, rate, &t.text)?);
                            words.push(shown);
                            words.push(t.text.clone());
                        } else {
                            words.push(shown + ".");
                        }
                        let phrase_word = plan.lead.as_ref().map_or(0, |l| l.text.split_whitespace().count());
                        (splice(&parts, &gaps, target), words.join(" "), phrase_word)
                    };
                    let refs = reference_matches(&text);
                    let positive = plan.phrase.is_some();
                    let expected_fire = positive && refs.iter().any(|(w, p)| *w == phrase_word && *p == plan.phrase.as_ref().unwrap().text);
                    // Level the speech, then three noise conditions of the same utterance.
                    let span = (spliced.speech_start, spliced.speech_end);
                    let gain = SPEECH_RMS / speech_power(&spliced.audio, span).sqrt().max(1e-9);
                    let mut speech = vec![0f32; LEAD_SILENCE];
                    speech.extend(spliced.audio.iter().map(|v| (*v as f64 * gain) as f32));
                    speech.extend(vec![0f32; TAIL_SILENCE]);
                    let span = (span.0 + LEAD_SILENCE, span.1 + LEAD_SILENCE);
                    let conditions = [("none", None), (NOISE10[base % 4], Some(10.0)), (NOISE_OTHER[base % 6].0, Some(NOISE_OTHER[base % 6].1))];
                    for (ci, (noise, snr)) in conditions.into_iter().enumerate() {
                        index += 1;
                        if !keep(index - 1) {
                            continue;
                        }
                        let mut nrng = Rng(seed ^ (base as u64 * 3 + ci as u64 + 7).wrapping_mul(0x2545_F491_4F6C_DD1D));
                        let n = speech.len();
                        let mut audio = speech.clone();
                        if let Some(snr) = snr {
                            let raw = match noise {
                                "white" => white(&mut nrng, n),
                                "pink" => pink(&mut nrng, n),
                                "music" => music(&mut nrng, n),
                                _ => {
                                    let pool = &babble_pool[lang];
                                    let talkers: Vec<Vec<f32>> = (0..5).map(|_| {
                                        // Each talker: three different renders back to back.
                                        let picks: Vec<String> = (0..3).map(|_| nrng.pick(pool).clone()).collect();
                                        picks.iter().flat_map(|p| tts.wav(p)).collect()
                                    }).collect();
                                    let refs: Vec<&[f32]> = talkers.iter().map(|t| t.as_slice()).collect();
                                    babble(&mut nrng, &refs, n)
                                }
                            };
                            if tts.listed.is_some() {
                                continue;
                            }
                            let g = noise_gain(&speech, span, &raw, snr);
                            // Room noise runs until a second after speech, then fades over 0.5 s so the turn can end.
                            let hold_until = span.1 + RATE;
                            for (i, a) in audio.iter_mut().enumerate() {
                                let env = if i < hold_until { 1.0 } else { (1.0 - (i - hold_until) as f64 / (RATE / 2) as f64).max(0.0) };
                                *a += (raw[i] as f64 * g * env) as f32;
                            }
                        }
                        let peak = audio.iter().fold(0f32, |m, v| m.max(v.abs()));
                        if peak > 0.98 {
                            audio.iter_mut().for_each(|v| *v *= 0.98 / peak);
                        }
                        cases.push(Case {
                            id: format!("{lang}-{}-{}-{rate}-{}-{}-{}", v.engine, v.voice, index - 1, plan.position, noise),
                            lang: lang.into(),
                            engine: v.engine.clone(),
                            voice: v.voice.clone(),
                            rate: rate.into(),
                            position: plan.position.into(),
                            phrase: plan.phrase.as_ref().map(|p| p.text.clone()),
                            text: text.clone(),
                            noise: noise.into(),
                            snr_db: snr,
                            expected_fire,
                            reference_fires: !refs.is_empty(),
                            audio,
                            onset: positive.then_some(spliced.target_onset + LEAD_SILENCE),
                            speech: span,
                            start_ns: 0,
                        });
                    }
                    base += 1;
                }
                combo += 1;
            }
        }
    }
    if let (Some(out), Some(listed)) = (listed, tts.listed) {
        *out = listed;
    }
    Ok(cases)
}

fn png_rgb(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    let mut reader = png::Decoder::new(std::io::BufReader::new(File::open(path)?)).read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("png size")?];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    ensure!(info.color_type == png::ColorType::Rgb, "shot is not RGB");
    Ok((buf, info.width, info.height))
}

/// Records the synthetic screen through the facade in host-time order, interleaved with the audio.
struct Recorder {
    next: u64,
    frames: u64,
    period: u64,
    w: u32,
    h: u32,
    display: screen::DisplayInfo,
    draw_s: f64,
    push_s: f64,
    push_ms: Vec<f64>,
    block: Instant,
}
impl Recorder {
    /// Push every frame whose capture time is at or before `t_ns`.
    fn until(&mut self, buffer: &Arc<Mutex<screen::ScreenBuffer>>, t_ns: u64) -> Result<()> {
        while self.next < self.frames && SCREEN_START_NS + self.next * self.period <= t_ns {
            if self.next > 0 && self.next % 500 == 0 {
                self.push_ms.push(self.block.elapsed().as_secs_f64() * 1000.0 / 500.0);
                self.block = Instant::now();
            }
            let t = Instant::now();
            let frame = screen::Frame::new(self.display.clone(), SCREEN_START_NS + self.next * self.period, self.w, self.h, frame_rgb(self.next, self.w, self.h))?;
            self.draw_s += t.elapsed().as_secs_f64();
            let t = Instant::now();
            buffer.lock().unwrap().push(frame)?;
            self.push_s += t.elapsed().as_secs_f64();
            self.next += 1;
        }
        Ok(())
    }
}

/// One case's listen events, handled the way the app's pipeline does.
struct CaseCtx<'a> {
    source: &'a str,
    case: &'a Case,
    probs: &'a [f32],
    gt: Option<u64>,
    period: u64,
    end_ns: u64,
    shots: &'a Path,
    matcher: screen::PhraseMatcher,
    prefix: Vec<Word>,
    finals: Vec<(Vec<Word>, u64, bool)>,
    transcript: Vec<String>,
    matches: Vec<Value>,
    partial_matches: Vec<Value>,
    partial_seen: std::collections::HashSet<(String, u64)>,
}
impl CaseCtx<'_> {
    fn handle(&mut self, events: Vec<ListenEvent>, forced: bool, now: u64, buffer: &Arc<Mutex<screen::ScreenBuffer>>, holds: &mut screen::ScreenHolds) -> Result<()> {
        for e in events {
            match e {
                ListenEvent::TranscriptSegment { words, .. } => self.prefix.extend(words),
                ListenEvent::PartialTranscript { words, .. } => {
                    let mut all = self.prefix.clone();
                    all.extend(words);
                    let t: Vec<screen::TimedWord> = all.iter().map(screen::TimedWord::from).collect();
                    for m in screen::PhraseMatcher::new(screen::SCREEN_PHRASES).observe_utterance(self.source, "partial", &t, true) {
                        if self.partial_seen.insert((m.phrase.clone(), m.start_ns)) {
                            self.partial_matches.push(json!({"phrase":m.phrase,"start_ns":m.start_ns,"trigger_ns":m.trigger_ns}));
                        }
                    }
                }
                ListenEvent::FinalUtterance { words, end_ns, .. } => {
                    let mut all = std::mem::take(&mut self.prefix);
                    all.extend(words);
                    self.final_(all, end_ns, forced, now, buffer, holds)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The app's final path: matcher per utterance, the decision's phrase start, pin_now, attach_held.
    fn final_(&mut self, words: Vec<Word>, final_end: u64, forced: bool, now: u64, buffer: &Arc<Mutex<screen::ScreenBuffer>>, holds: &mut screen::ScreenHolds) -> Result<()> {
        let u = self.finals.len();
        self.transcript.push(listen::Transcript::from_words(words.clone()).text);
        let t: Vec<screen::TimedWord> = words.iter().map(screen::TimedWord::from).collect();
        let utterance_start = t.first().map_or(0, |w| w.start_ns);
        let now = now.min(self.end_ns);
        for m in self.matcher.observe_utterance(self.source, &u.to_string(), &t, true) {
            let end = m.trigger_ns.saturating_add(SIDE_AFTER_NS);
            let from = end.saturating_sub(SIDE_MAX_NS).max(utterance_start);
            let window: Vec<screen::TimedWord> = t.iter().filter(|w| w.end_ns > from && w.start_ns <= end).cloned().collect();
            let phrase_start = screen::PhraseMatcher::new(screen::SCREEN_PHRASES)
                .observe_utterance(self.source, "decision", &window, true)
                .into_iter()
                .min_by_key(|d| d.trigger_ns.abs_diff(m.trigger_ns))
                .map_or(m.trigger_ns.saturating_sub(PHRASE_FALLBACK_NS), |d| d.start_ns.min(m.trigger_ns));
            // Through the production hold/attach path at a given phrase start; read the frame back.
            let mut select = |start: u64| -> Result<Value> {
                let matched = screen::PhraseMatch { start_ns: start, ..m.clone() };
                let hold = holds.pin_now(buffer, self.source, m.trigger_ns, start, now);
                Ok(match screen::attach_held(hold, &matched, now, self.shots) {
                    Ok(action) => {
                        let shot = &action.shots[0];
                        let (rgb, sw, sh) = png_rgb(&shot.path)?;
                        let decoded = read_frame_index(&rgb, sw, sh);
                        for s in &action.shots {
                            let _ = fs::remove_file(&s.path);
                        }
                        json!({"start_ns":start,"decoded_frame":decoded,"shot_frame_ns":shot.frame_ns,"shot_size":[sw,sh]})
                    }
                    Err(e) => json!({"start_ns":start,"error":format!("{e:#}")}),
                })
            };
            let selection = select(phrase_start)?;
            // Candidate corrections (evaluation only): snap into speech within the first phrase word.
            let first_end = t.iter().find(|w| w.start_ns == m.start_ns).map_or(m.trigger_ns, |w| w.end_ns);
            let vad = vad_snap(self.probs, self.case.start_ns, phrase_start, first_end, 0.5);
            let energy = energy_snap(&self.case.audio, self.case.start_ns, phrase_start, first_end);
            let vad_selection = select(vad)?;
            let energy_selection = select(energy)?;
            let gt = self.gt;
            let err = |x: u64| gt.map(|g| (x as f64 - g as f64) / 1e6);
            self.matches.push(json!({
                "phrase": m.phrase, "utterance": u, "forced_flush": forced,
                "start_ns": m.start_ns, "trigger_ns": m.trigger_ns, "phrase_start_ns": phrase_start,
                "first_word_end_ns": first_end, "final_end_ns": final_end, "decided_ns": now,
                "error_ms": err(phrase_start),
                "frame_at_phrase_start": frame_at(phrase_start, SCREEN_START_NS, self.period),
                "vad_snap": {"error_ms": err(vad), "selection": vad_selection},
                "energy_snap": {"error_ms": err(energy), "selection": energy_selection},
                "selection": selection,
            }));
        }
        self.finals.push((words, final_end, forced));
        Ok(())
    }
}

pub fn run(root: &Path, mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut out = root.join("lab/eval/data/screen-bench/run");
    let mut tts = root.join("lab/eval/data/screen-bench/tts");
    let mut models = root.join("models");
    let (mut seed, mut limit, mut only, mut keep, mut stride, mut offset) = (22u64, usize::MAX, None::<String>, false, 1usize, 0usize);
    let mut list_tts = None::<PathBuf>;
    let mut policy = "final".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = args.next().context("--out")?.into(),
            "--tts" => tts = args.next().context("--tts")?.into(),
            "--models" => models = args.next().context("--models")?.into(),
            "--seed" => seed = args.next().context("--seed")?.parse()?,
            "--limit" => limit = args.next().context("--limit")?.parse()?,
            "--stride" => stride = args.next().context("--stride")?.parse()?,
            // Shard k of n: --offset k --stride n (each shard records its own screen archive).
            "--offset" => offset = args.next().context("--offset")?.parse()?,
            "--only" => only = Some(args.next().context("--only")?),
            "--policy" => policy = args.next().context("--policy")?,
            "--keep-archive" => keep = true,
            // Write the renders this shard reads (one path per line) and stop: gen_tts.py --need synthesizes only those.
            "--list-tts" => list_tts = Some(args.next().context("--list-tts")?.into()),
            "--reanalyze" => return reanalyze(root, &tts, &models, &PathBuf::from(args.next().context("--reanalyze")?), seed),
            // Offline onset studies: each recorded case's audio (16 kHz mono 16-bit WAV) and Silero probabilities.
            "--dump-audio" => return dump_audio(root, &tts, &models, &PathBuf::from(args.next().context("--dump-audio")?), seed),
            _ => bail!("Unknown argument: {a}"),
        }
    }
    ensure!(matches!(policy.as_str(), "final" | "responsive"), "--policy final|responsive");
    let stride = stride.max(1);
    let in_shard = move |i: usize| i >= offset && (i - offset) % stride == 0 && (i - offset) / stride < limit;
    if let Some(path) = list_tts {
        let mut listed = Default::default();
        build_cases(root, &tts, seed, only.as_deref(), &in_shard, Some(&mut listed))?;
        fs::write(&path, listed.iter().map(|p| format!("{p}\n")).collect::<String>())?;
        eprintln!("{} renders for shard {offset} of {stride}", listed.len());
        return Ok(());
    }
    let out = heimdall_eval::prepare_output(root, &out)?;
    let t0 = Instant::now();
    let mut cases = build_cases(root, &tts, seed, only.as_deref(), &in_shard, None)?;
    eprintln!("cases={} built in {:.1}s", cases.len(), t0.elapsed().as_secs_f64());

    // One host timeline: utterance blocks back to back after a 1 s screen lead.
    let mut cursor = SCREEN_START_NS + 1_000_000_000;
    let mut jitter = Rng(seed ^ 0xF00D);
    for c in &mut cases {
        cursor += jitter.range(0, 199) as u64 * 1_000_000; // random phase against the 200 ms frames
        c.start_ns = cursor;
        cursor += c.audio.len() as u64 * NS_PER_SAMPLE;
    }
    let end_ns = cursor + 1_000_000_000;

    // Screen: the real recording facade over an isolated archive store, hardware HEVC, app defaults.
    let config = screen::ScreenConfig::default();
    let period = 1_000_000_000 / config.fps as u64;
    let display = screen::DisplayInfo { id: 1, x: 0.0, y: 0.0, width: 3456, height: 2234 };
    let (w, h) = config.size(display.width, display.height);
    let store = out.join(format!("archive-{offset}of{stride}"));
    if store.exists() {
        fs::remove_dir_all(&store)?;
    }
    fs::create_dir_all(&store)?;
    let shots = out.join(format!("shots-{offset}of{stride}"));
    if shots.exists() {
        fs::remove_dir_all(&shots)?;
    }
    let mut buffer = screen::ScreenBuffer::recording(config.clone(), store.join("session-screen-bench"))?;
    buffer.set_displays(vec![display.clone()]);
    let frames = (end_ns - SCREEN_START_NS) / period + 1;
    ensure!(frames < 1 << 16, "frame index is 16 bits: shard the run (--offset/--stride)");
    let buffer = Arc::new(Mutex::new(buffer));
    let mut holds = screen::ScreenHolds::new(None); // no native grab: never touches the real screen
    // Live order: the screen is recorded up to each audio chunk's time before that chunk is heard, and
    // a command is selected the moment its final is emitted, from the still-open recording segment.
    let mut recorder = Recorder { next: 0, frames, period, w, h, display: display.clone(), draw_s: 0.0, push_s: 0.0, push_ms: vec![], block: Instant::now() };

    // Listen: the app's engine construction (Metal Parakeet and turn backend, the user's turn policy).
    let asr = listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), "metal")?;
    let mut engine = ListenEngine::load_with_turn_backend(&models, asr, Some("metal"))?;
    let mut silero = listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?;
    let mut results = BufWriter::new(File::create(out.join(format!("cases-{policy}-{offset}of{stride}.jsonl")))?);
    let t_asr = Instant::now();
    let mut audio_s = 0.0;
    for (i, c) in cases.iter().enumerate() {
        let source = format!("mic:bench-{i}");
        if policy == "responsive" {
            engine.set_source_policy(&source, listen::PartialPolicy::Responsive)?;
        }
        // Silero over the case audio in the engine's 512-sample blocks (fresh state per source, like the engine).
        let mut vad_state = listen::onnx::VadState::default();
        let probs: Vec<f32> = c.audio.chunks_exact(512).map(|b| silero.probability(b.try_into().unwrap(), &mut vad_state)).collect::<Result<_>>()?;
        let gt = c.onset.map(|o| sample_ns(c.start_ns, o));
        let mut ctx = CaseCtx { source: &source, case: c, probs: &probs, gt, period, end_ns, shots: &shots,
            matcher: screen::PhraseMatcher::default(), prefix: vec![], finals: vec![], transcript: vec![], matches: vec![],
            partial_matches: vec![], partial_seen: Default::default() };
        let chunks: Vec<&[f32]> = c.audio.chunks(320).collect();
        for (k, chunk) in chunks.iter().enumerate() {
            let at = c.start_ns + (k * 320) as u64 * NS_PER_SAMPLE;
            recorder.until(&buffer, at)?;
            let events = engine.push(AudioChunk { source: &source, generation: 0, host_time_ns: at, samples: chunk })?;
            let now = at + chunk.len() as u64 * NS_PER_SAMPLE;
            ctx.handle(events, false, now, &buffer, &mut holds)?;
        }
        let case_end = c.start_ns + c.audio.len() as u64 * NS_PER_SAMPLE;
        recorder.until(&buffer, case_end)?;
        let flushed = engine.flush_source(&source)?;
        ctx.handle(flushed, true, case_end, &buffer, &mut holds)?;
        let _ = engine.remove_source(&source);
        audio_s += c.audio.len() as f64 / RATE as f64;
        let partials: Vec<Value> = std::mem::take(&mut ctx.partial_matches).into_iter().map(|mut p| {
            if let Some(g) = gt { p["error_ms"] = json!((p["start_ns"].as_u64().unwrap() as f64 - g as f64) / 1e6); }
            p
        }).collect();
        let row = json!({
            "id": c.id, "lang": c.lang, "engine": c.engine, "voice": c.voice, "rate": c.rate,
            "position": c.position, "phrase": c.phrase, "text": c.text, "noise": c.noise, "snr_db": c.snr_db,
            "expected_fire": c.expected_fire, "reference_fires": c.reference_fires,
            "onset_ns": gt, "start_ns": c.start_ns,
            "speech_start_ns": sample_ns(c.start_ns, c.speech.0), "speech_end_ns": sample_ns(c.start_ns, c.speech.1),
            "expected_frame": gt.and_then(|g| frame_at(g, SCREEN_START_NS, period)),
            "onset_phase_ms": gt.map(|g| ((g - SCREEN_START_NS) % period) as f64 / 1e6),
            "finals": ctx.finals.len(), "forced_flush": ctx.finals.iter().any(|f| f.2),
            "transcript": ctx.transcript, "matches": ctx.matches, "partial_matches": partials,
            "words": ctx.finals.iter().map(|f| f.0.iter().map(|w| json!([w.text, w.start_ns, w.end_ns])).collect::<Vec<_>>()).collect::<Vec<_>>(),
        });
        writeln!(results, "{row}")?;
        if (i + 1) % 50 == 0 {
            results.flush()?;
            eprintln!("{} / {} cases, {:.1}s", i + 1, cases.len(), t_asr.elapsed().as_secs_f64());
        }
    }
    recorder.until(&buffer, end_ns)?;
    let (frames_pushed, draw_s, push_s, push_ms) = (recorder.next, recorder.draw_s, recorder.push_s, recorder.push_ms.clone());
    results.flush()?;
    let asr_s = t_asr.elapsed().as_secs_f64() - push_s - draw_s;
    fs::write(out.join(format!("run-{policy}-{offset}of{stride}.json")), serde_json::to_vec_pretty(&json!({
        "cases": cases.len(), "seed": seed, "stride": stride, "offset": offset, "only": only, "policy": policy,
        "screen": {"frames": frames_pushed, "width": w, "height": h, "fps": config.fps, "push_s": push_s, "draw_s": draw_s, "ms_per_frame_by_500": push_ms},
        "audio_s": audio_s, "listen_and_select_s": asr_s, "models": models,
    }))?)?;
    drop(holds);
    drop(buffer);
    if !keep {
        fs::remove_dir_all(&store)?;
        let _ = fs::remove_dir_all(&shots);
    }
    eprintln!("done: {} cases, audio {:.0}s, listen+select {:.1}s", cases.len(), audio_s, asr_s);
    Ok(())
}

/// Evaluation-only: rebuild the recorded cases of a finished run (no ASR, no screen) and write each one's audio as
/// `<run>/audio/<id>.wav` plus `<run>/audio/silero.jsonl` (per-case 32 ms block probabilities, state reset per case), so
/// onset corrections can be scored offline against the run's word times.
fn dump_audio(root: &Path, tts: &Path, models: &Path, run: &Path, seed: u64) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for entry in fs::read_dir(run)? {
        let path = entry?.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("cases-final-") && n.ends_with(".jsonl")) {
            for line in fs::read_to_string(&path)?.lines() {
                let v: Value = serde_json::from_str(line)?;
                ids.insert(v["id"].as_str().unwrap_or_default().to_string());
            }
        }
    }
    let cases = build_cases(root, tts, seed, None, &|_| true, None)?;
    let dir = run.join("audio");
    fs::create_dir_all(&dir)?;
    let mut silero = listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?;
    let mut out = BufWriter::new(File::create(dir.join("silero.jsonl"))?);
    let spec = hound::WavSpec { channels: 1, sample_rate: RATE as u32, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    for c in cases.iter().filter(|c| ids.contains(&c.id)) {
        let mut w = hound::WavWriter::create(dir.join(format!("{}.wav", c.id)), spec)?;
        for v in &c.audio {
            w.write_sample((v.clamp(-1.0, 1.0) * 32767.0).round() as i16)?;
        }
        w.finalize()?;
        let mut state = listen::onnx::VadState::default();
        let probs: Vec<f32> = c.audio.chunks_exact(512).map(|b| silero.probability(b.try_into().unwrap(), &mut state)).collect::<Result<_>>()?;
        writeln!(out, "{}", json!({"id": c.id, "probs": probs}))?;
    }
    out.flush()?;
    Ok(())
}

/// Evaluation-only follow-up on finished shards (no ASR, no screen): rebuild the same cases, run Silero, and
/// score a VAD snap bounded by the phrase's last word instead of its first, with the frame-on-screen rule
/// (the archive selection agreed with that rule in every recorded selection). Writes vad-reanalysis.jsonl.
fn reanalyze(root: &Path, tts: &Path, models: &Path, run: &Path, seed: u64) -> Result<()> {
    let cases = build_cases(root, tts, seed, None, &|_| true, None)?;
    let mut rows: HashMap<String, Value> = HashMap::new();
    for entry in fs::read_dir(run)? {
        let path = entry?.path();
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("cases-final-") && n.ends_with(".jsonl")) {
            for line in fs::read_to_string(&path)?.lines() {
                let v: Value = serde_json::from_str(line)?;
                rows.insert(v["id"].as_str().unwrap_or_default().to_string(), v);
            }
        }
    }
    let mut silero = listen::onnx::Silero::load(&models.join("silero/silero_vad.onnx"))?;
    let mut out = BufWriter::new(File::create(run.join("vad-reanalysis.jsonl"))?);
    for c in &cases {
        let Some(row) = rows.get(&c.id) else { continue };
        let Some(start_ns) = row["start_ns"].as_u64() else { continue };
        let mut state = listen::onnx::VadState::default();
        let probs: Vec<f32> = c.audio.chunks_exact(512).map(|b| silero.probability(b.try_into().unwrap(), &mut state)).collect::<Result<_>>()?;
        let snaps: Vec<Value> = row["matches"].as_array().into_iter().flatten().map(|m| {
            let start = m["phrase_start_ns"].as_u64().unwrap_or(0);
            let trigger = m["trigger_ns"].as_u64().unwrap_or(start);
            let mut v = json!({"trigger_ns": trigger});
            for th in [0.5f32, 0.7] {
                let snapped = vad_snap(&probs, start_ns, start, trigger.max(start + 1), th);
                v[format!("vad_to_trigger_{th}")] = json!({"start_ns": snapped, "frame": frame_at(snapped, SCREEN_START_NS, 200_000_000)});
            }
            v
        }).collect();
        writeln!(out, "{}", json!({"id": c.id, "snaps": snaps}))?;
    }
    out.flush()?;
    Ok(())
}
