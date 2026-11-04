//! Per-word attribution; separator text is evidence only.
mod replay;
use super::*;
use heimdall_night::correction::{Input, Output};
pub(crate) use replay::run as replay;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Run {
    pub speaker: String,
    pub track: Option<TrackId>,
    pub words: Vec<Word>,
}
impl Run {
    pub fn text(&self) -> String {
        listen::Transcript::from_words(self.words.clone()).text
    }
}
#[derive(Default)]
pub(super) struct State {
    timelines: HashMap<(String, u64), Timeline>,
    finals: BTreeMap<u64, Final>,
    submitted: HashSet<String>,
    runtime: Option<crate::speech_split::Runtime>,
    inflight: HashMap<u64, (String, usize)>,
    decoded: HashMap<String, Decoding>,
    active: HashSet<(String, u64)>,
    outstanding: usize,
    pending: BTreeMap<String, Pending>,
    reserved_bytes: usize,
    reservations: HashMap<String, usize>,
    dirty: HashSet<u64>,
    names: BTreeMap<TrackId, String>,
    job_calls: HashMap<String, (String, u64)>,
    call_cpu: HashMap<(String, u64), f64>,
    call_asr_cpu: HashMap<(String, u64), f64>,
}
#[derive(Default)]
struct Timeline {
    scanned: u64,
    segments: Vec<speakers::diar::SpeakerSegment>,
    finalized: Vec<speakers::diar::SpeakerSegment>,
}
struct Final {
    source: String,
    origin: u64,
    start: u64,
    end: u64,
    words: Vec<Word>,
    notes: Vec<String>,
    last: String,
    projected: Vec<Run>,
    revision: u64,
}
struct Decoding {
    input: Input,
    output: Output,
    words: [Option<Vec<Word>>; 2],
    admitted: Instant,
}

/// Only a strict majority of exclusive time can own a word. Any simultaneous
/// speech makes that portion unowned; ties, zero-duration and missing data stay Unknown.
pub(super) fn owner(
    segments: &[speakers::diar::SpeakerSegment],
    origin: u64,
    word: &Word,
) -> Option<i32> {
    if word.end_ns <= word.start_ns {
        return None;
    }
    let a = word.start_ns.saturating_sub(origin) as f64 / 1e9;
    let b = word.end_ns.saturating_sub(origin) as f64 / 1e9;
    let mut edges = vec![a, b];
    for s in segments {
        if s.end_time > a && s.start_time < b {
            edges.push(s.start_time.max(a));
            edges.push(s.end_time.min(b));
        }
    }
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    let mut totals = HashMap::<i32, f64>::new();
    for span in edges.windows(2) {
        let mid = (span[0] + span[1]) / 2.;
        let active: BTreeSet<_> = segments
            .iter()
            .filter(|s| s.start_time <= mid && s.end_time > mid)
            .map(|s| s.speaker)
            .collect();
        if active.len() == 1 {
            *totals.entry(*active.first().unwrap()).or_default() += span[1] - span[0];
        }
    }
    totals
        .into_iter()
        .find(|(_, seconds)| *seconds > (b - a) / 2. + 1e-9)
        .map(|(track, _)| track)
}
// A change needs evidence from owned words, not silence or attached ambiguity.
const MIN_RUN_WORDS: usize = 4;
const MIN_RUN_EXCLUSIVE_SECONDS: f64 = 1.0;

struct WordRun {
    start: usize,
    end: usize,
    track: Option<i32>,
}

fn sentence_end(word: &Word) -> bool {
    word.text
        .trim_end_matches(|c: char| {
            c.is_whitespace() || matches!(c, '\"' | '\'' | '”' | '’' | ')' | ']')
        })
        .ends_with(['.', '?', '!', '…'])
}

/// Compare gaps at the two boundaries of a contiguous run. A sentence ending
/// immediately before it breaks an equal-distance tie in favour of the next run.
fn neighbour(
    words: &[Word],
    run: &WordRun,
    left: Option<&WordRun>,
    right: Option<&WordRun>,
) -> Option<usize> {
    match (left, right) {
        (Some(l), Some(r)) => {
            let before = words[run.start]
                .start_ns
                .saturating_sub(words[l.end - 1].end_ns);
            let after = words[r.start]
                .start_ns
                .saturating_sub(words[run.end - 1].end_ns);
            Some(
                if after < before || (after == before && sentence_end(&words[l.end - 1])) {
                    r.start
                } else {
                    l.start
                },
            )
        }
        (Some(l), None) => Some(l.start),
        (None, Some(r)) => Some(r.start),
        (None, None) => None,
    }
}

fn grouped(owners: &[Option<i32>]) -> Vec<WordRun> {
    let mut runs: Vec<WordRun> = vec![];
    for (i, track) in owners.iter().enumerate() {
        if let Some(last) = runs.last_mut().filter(|r| r.track == *track) {
            last.end = i + 1;
        } else {
            runs.push(WordRun {
                start: i,
                end: i + 1,
                track: *track,
            });
        }
    }
    runs
}

/// Union of actual exclusive speech intersecting the original owned words.
/// Overlapping ASR timestamps and duplicate diarizer segments count only once.
fn exclusive_seconds(
    words: &[&Word],
    track: i32,
    origin: u64,
    segments: &[speakers::diar::SpeakerSegment],
) -> f64 {
    let mut edges: Vec<_> = words
        .iter()
        .flat_map(|w| {
            [
                w.start_ns.saturating_sub(origin) as f64 / 1e9,
                w.end_ns.saturating_sub(origin) as f64 / 1e9,
            ]
        })
        .collect();
    for segment in segments {
        edges.extend([segment.start_time, segment.end_time]);
    }
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    edges
        .windows(2)
        .filter_map(|span| {
            let mid = (span[0] + span[1]) / 2.;
            let in_word = words.iter().any(|w| {
                w.start_ns.saturating_sub(origin) as f64 / 1e9 <= mid
                    && w.end_ns.saturating_sub(origin) as f64 / 1e9 > mid
            });
            let active: BTreeSet<_> = segments
                .iter()
                .filter(|s| s.start_time <= mid && s.end_time > mid)
                .map(|s| s.speaker)
                .collect();
            (in_word && active.len() == 1 && active.contains(&track)).then_some(span[1] - span[0])
        })
        .sum()
}

pub(super) fn word_runs(
    words: &[Word],
    source: &str,
    origin: u64,
    segments: &[speakers::diar::SpeakerSegment],
    names: &BTreeMap<TrackId, String>,
) -> Vec<Run> {
    let owners: Vec<_> = words
        .iter()
        .map(|word| owner(segments, origin, word))
        .collect();
    let mut labels = owners.clone();
    let raw = grouped(&owners);
    // Unowned words cannot create a line. Attach an entire ambiguous run using
    // its actual boundary gaps, then reunite the same track on either side.
    for (i, run) in raw.iter().enumerate().filter(|(_, r)| r.track.is_none()) {
        if let Some(at) = neighbour(
            words,
            run,
            i.checked_sub(1).map(|i| &raw[i]),
            raw.get(i + 1),
        ) {
            labels[run.start..run.end].fill(owners[at]);
        }
    }
    let joined = grouped(&labels);
    let anchors: Vec<_> = joined
        .iter()
        .filter(|run| {
            let Some(track) = run.track else {
                return false;
            };
            let owned: Vec<_> = (run.start..run.end)
                .filter(|i| owners[*i] == Some(track))
                .map(|i| &words[i])
                .collect();
            let count: usize = owned
                .iter()
                .map(|w| {
                    w.text
                        .split_whitespace()
                        .filter(|t| t.chars().any(char::is_alphanumeric))
                        .count()
                })
                .sum();
            count >= MIN_RUN_WORDS
                && exclusive_seconds(&owned, track, origin, segments) + 1e-9
                    >= MIN_RUN_EXCLUSIVE_SECONDS
        })
        .collect();
    if anchors.is_empty() {
        // No defensible turn boundary: keep one readable line, using the track
        // with most exclusive evidence (first occurrence breaks exact ties).
        let mut seen = BTreeSet::new();
        let mut best = None;
        let mut most = 0.;
        for track in owners.iter().flatten().filter(|t| seen.insert(**t)) {
            let owned: Vec<_> = words
                .iter()
                .zip(&owners)
                .filter(|(_, t)| **t == Some(*track))
                .map(|(w, _)| w)
                .collect();
            let seconds = exclusive_seconds(&owned, *track, origin, segments);
            if seconds > most + 1e-9 {
                best = Some(*track);
                most = seconds;
            }
        }
        labels.fill(best);
    } else {
        for run in &joined {
            if anchors.iter().any(|a| a.start == run.start) {
                continue;
            }
            let left = anchors.iter().rev().find(|a| a.end <= run.start).copied();
            let right = anchors.iter().find(|a| a.start >= run.end).copied();
            if let Some(at) = neighbour(words, run, left, right) {
                let track = labels[at];
                labels[run.start..run.end].fill(track);
            }
        }
    }
    grouped(&labels)
        .iter()
        .map(|run| {
            let track = run.track.map(|speaker| TrackId {
                source: source.into(),
                session: origin,
                speaker,
            });
            let speaker = track
                .as_ref()
                .and_then(|t| names.get(t))
                // Anonymous track names ("Speaker 2", "Speaker ?") say nothing; a coded voice's label is its name.
                .filter(|name| !name.is_empty() && name.as_str() != "Speaker ?" && super::transcript_naming::unidentified_speaker(name).is_none())
                .cloned()
                .unwrap_or_else(|| "Unknown".into());
            Run {
                speaker,
                track,
                words: words[run.start..run.end].to_vec(),
            }
        })
        .collect()
}
pub(super) fn context_text(runs: &[Run]) -> (String, String) {
    if let [run] = runs {
        (run.speaker.clone(), run.text())
    } else {
        (
            "Call".into(),
            runs.iter()
                .map(|r| format!("{}: {}", r.speaker, r.text()))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}
fn origin_ns(origin: u64, seconds: f64) -> u64 {
    origin + (seconds * 1e9).round() as u64
}
/// Stable two-track regions. Pair changes and >=3-way spans split ownership;
/// right context is required before admission. No clock holes are filled.
fn overlaps(
    segments: &[speakers::diar::SpeakerSegment],
    origin: u64,
    until: u64,
) -> Vec<(u64, u64, Vec<i32>)> {
    let mut edges = vec![];
    for s in segments {
        edges.push(origin + (s.start_time * 1e9).round() as u64);
        edges.push(origin + (s.end_time * 1e9).round() as u64);
    }
    edges.sort();
    edges.dedup();
    let mut out: Vec<(u64, u64, Vec<i32>)> = vec![];
    for span in edges.windows(2) {
        let finish = span[1].min(until);
        if span[0] >= finish {
            continue;
        }
        let mid = (span[0] - origin) as f64 / 1e9 + (finish - span[0]) as f64 / 2e9;
        let active: Vec<_> = segments
            .iter()
            .filter(|s| s.start_time <= mid && s.end_time > mid)
            .map(|s| s.speaker)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if active.len() != 2 {
            continue;
        }
        if let Some(last) = out.last_mut().filter(|r| r.1 == span[0] && r.2 == active) {
            last.1 = finish;
        } else {
            out.push((span[0], finish, active));
        }
    }
    out
}
// At most two CPU/ASR jobs may retain PCM anywhere in the parent/worker lanes.
const MAX_RETAINED_JOBS: usize = 2;
const JOB_BYTES: usize = 3 * 128000 * std::mem::size_of::<f32>();
const MAX_RETAINED_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone)]
struct Pending {
    source: String,
    origin: u64,
    start: u64,
    end: u64,
}
fn normalized(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}
fn evidence_tracks(
    words: &[Option<Vec<Word>>; 2],
    output: &Output,
    a: u64,
    b: u64,
    originals: &[Word],
) -> Vec<serde_json::Value> {
    let owned: Vec<Vec<Word>> = words
        .iter()
        .map(|words| {
            words
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|w| {
                    let mid = w.start_ns + w.end_ns.saturating_sub(w.start_ns) / 2;
                    mid >= a && mid < b
                })
                .cloned()
                .collect()
        })
        .collect();
    owned.iter().enumerate().map(|(slot, words)| {
        let mean = if words.is_empty() { 0. } else { words.iter().map(|w|w.confidence as f64).sum::<f64>() / words.len() as f64 };
        let tokens: Vec<_> = words.iter().flat_map(|w| w.text.split_whitespace()).map(normalized).filter(|w| !w.is_empty()).collect();
        let mut reasons = vec![];
        if words.len() < 2 { reasons.push("fewer_than_two_owned_words"); }
        if mean < 0.85 || !mean.is_finite() { reasons.push("mean_confidence_below_0.85"); }
        if words.iter().any(|w| !w.confidence.is_finite() || w.confidence < 0.5) { reasons.push("word_confidence_below_0.5"); }
        if tokens.windows(2).any(|p|p[0]==p[1]) { reasons.push("identical_word_repetition"); }
        let other: Vec<_> = owned[1-slot].iter().flat_map(|w|w.text.split_whitespace()).map(normalized).filter(|w|!w.is_empty()).collect();
        if !tokens.is_empty() && tokens == other { reasons.push("duplicate_recovered_track"); }
        let original_owned: Vec<_> = originals.iter().filter(|w| { let mid=w.start_ns+w.end_ns.saturating_sub(w.start_ns)/2; mid>=a&&mid<b }).cloned().collect();
        let original_text=listen::Transcript::from_words(original_owned.clone()).text;
        let candidate_text=listen::Transcript::from_words(words.clone()).text;
        let edit_distance=heimdall_night::metrics::wer(&original_text,&candidate_text);
        let mut proposed_final: Vec<_> = originals.iter().filter(|w| { let mid=w.start_ns+w.end_ns.saturating_sub(w.start_ns)/2; mid<a||mid>=b }).cloned().collect();
        proposed_final.extend(words.clone());proposed_final.sort_by_key(|w|w.start_ns);
        serde_json::json!({"candidate_edit":{"start_ns":a,"end_ns":b,"original_owned_words":original_owned,"word_edits":edit_distance.errors,"proposed_final":listen::Transcript::from_words(proposed_final),"applied":false},"slot":slot,"owned_words":words,"owned_text":listen::Transcript::from_words(words.clone()).text,"identity":output.tracks[slot].identity,"mean_confidence":mean,"would_publish":reasons.is_empty(),"reasons":reasons,"gate":"diagnostic_v1_unvalidated","original_context_words":originals,"identity_is_not_text_quality":true,"speech_support":"not_measured; output_RMS_normalized","published":false})
    }).collect()
}
impl Pipeline {
    pub(super) fn call_final(
        &mut self,
        id: u64,
        source: &str,
        start: u64,
        end: u64,
        words: &[Word],
        notes: &[String],
    ) {
        if let Some(d) = self.discord.get(source) {
            self.speech_split.finals.insert(
                id,
                Final {
                    source: source.into(),
                    origin: d.origin,
                    start,
                    end,
                    words: words.to_vec(),
                    notes: notes.to_vec(),
                    last: String::new(),
                    projected: vec![],
                    revision: 0,
                },
            );
            self.speech_split.dirty.insert(id);
            while self.speech_split.finals.len() > 2048 {
                self.speech_split.finals.pop_first();
            }
        }
    }
    pub(super) fn stable_speech(
        &mut self,
        source: &str,
        origin: u64,
        until: u64,
        segments: Vec<speakers::diar::SpeakerSegment>,
        finalized: bool,
    ) -> Result<()> {
        let key = (source.to_owned(), origin);
        let timeline = self.speech_split.timelines.entry(key.clone()).or_default();
        let changed = if finalized {
            segments
                .first()
                .map_or(until, |s| origin + (s.start_time * 1e9).round() as u64)
        } else {
            timeline
                .segments
                .iter()
                .zip(&segments)
                .position(|(a, b)| {
                    a.speaker != b.speaker
                        || a.start_time != b.start_time
                        || a.end_time != b.end_time
                })
                .and_then(|i| {
                    segments
                        .get(i)
                        .map(|s| origin + (s.start_time * 1e9).round() as u64)
                })
                .unwrap_or_else(|| {
                    timeline
                        .segments
                        .last()
                        .map_or(origin, |s| origin + (s.end_time * 1e9).round() as u64)
                })
        };
        for (id, f) in &self.speech_split.finals {
            if f.source == source && f.origin == origin && f.end >= changed {
                self.speech_split.dirty.insert(*id);
            }
        }
        let scanned = timeline.scanned;
        if finalized {
            timeline.finalized.extend(segments);
        } else {
            timeline.segments = segments;
        }
        timeline.scanned = timeline.scanned.max(until.saturating_sub(1_000_000_000));
        let mut all = timeline.finalized.clone();
        all.extend(timeline.segments.clone());
        let scan_start = scanned.saturating_sub(8_000_000_000);
        all.retain(|segment| origin_ns(origin, segment.end_time) >= scan_start);
        self.speech_split.active.insert(key);
        for (mut a, end, pair) in overlaps(&all, origin, until) {
            while a < end {
                let b = (a + 6_000_000_000).min(end);
                if b.saturating_add(1_000_000_000) > until {
                    break;
                }
                if b <= scanned {
                    a = b;
                    continue;
                }
                let identity = format!("{source}:{origin}:{a}:{b}:{pair:?}");
                if !self.speech_split.submitted.contains(&identity)
                    && !self.speech_split.pending.contains_key(&identity)
                {
                    let pending = Pending {
                        source: source.into(),
                        origin,
                        start: a,
                        end: b,
                    };
                    // Metadata only: waveform retention starts after reserving a bounded slot.
                    self.index.evidence.event("speech_split_pending",source,&[],a,serde_json::json!({"job":identity,"owned_start_ns":a,"owned_end_ns":b,"call_origin_ns":origin}))?;
                    self.speech_split.pending.insert(identity, pending);
                }
                a = b;
            }
        }
        self.correction_admit()?;
        self.project_call(source)
    }
    fn release_correction(&mut self, key: &str) {
        if let Some(bytes) = self.speech_split.reservations.remove(key) {
            self.speech_split.reserved_bytes =
                self.speech_split.reserved_bytes.saturating_sub(bytes);
        }
        self.speech_split.job_calls.remove(key);
    }
    fn correction_admit(&mut self) -> Result<()> {
        // Reserve input + both maximum-sized outputs, including work inside the
        // event channel, before asking the CPU helper for anything.
        while self.speech_split.reservations.len() < MAX_RETAINED_JOBS
            && self.speech_split.reserved_bytes + JOB_BYTES <= MAX_RETAINED_BYTES
        {
            let Some((identity, pending)) = self.speech_split.pending.first_key_value() else {
                break;
            };
            let identity = identity.clone();
            let pending = pending.clone();
            if self.speech_split.runtime.is_none() {
                self.speech_split.runtime = Some(crate::speech_split::Runtime::start(
                    self.models.clone(),
                    self.internal.0.clone(),
                    self.session.clone(),
                    self.data.join("night/calls"),
                    self.tools.archive_floor.clone(),
                )?);
            }
            let input = Input {
                region: heimdall_night::dataset::Region {
                    session: self.session.file_name().unwrap().to_string_lossy().into(),
                    source: pending.source.clone(),
                    start_ns: pending.start,
                    end_ns: pending.end,
                    speakers: 2,
                    utterance_ids: vec![],
                    audio: vec![],
                },
                start_ns: pending.start.saturating_sub(1_000_000_000),
                samples: vec![],
                enrollments: self.load_store()?,
            };
            let result = self.speech_split.runtime.as_ref().unwrap().submit(
                identity.clone(),
                input,
                Instant::now(),
            );
            self.index.evidence.event("speech_split_admission",&pending.source,&[],pending.start,serde_json::json!({"job":identity,"admitted":result.is_ok(),"policy":"call_or_draining; battery/input/speech allowed","reason":result.as_ref().err().map(|e|e.to_string())}))?;
            if result.is_err() {
                break;
            } // remains pending; next tick retries even after expiry
            self.speech_split.submitted.insert(identity.clone());
            self.speech_split.pending.remove(&identity);
            self.speech_split.outstanding += 1;
            self.speech_split.reserved_bytes += JOB_BYTES;
            self.speech_split
                .reservations
                .insert(identity.clone(), JOB_BYTES);
            self.speech_split
                .job_calls
                .insert(identity, (pending.source, pending.origin));
        }
        Ok(())
    }
    pub(crate) fn correction_ready(
        &mut self,
        key: String,
        mut input: Input,
        result: Result<Output, String>,
        admitted: Instant,
    ) -> Result<()> {
        self.speech_split.outstanding = self.speech_split.outstanding.saturating_sub(1);
        match result {
            Ok(output) => {
                anyhow::ensure!(
                    output.tracks.len() == 2
                        && output.tracks.iter().all(|t| t.samples.len() <= 128000),
                    "invalid correction output bounds"
                );
                let retained_pcm = self
                    .speech_split
                    .decoded
                    .values()
                    .map(|job| {
                        job.output
                            .tracks
                            .iter()
                            .map(|t| t.samples.len() * 4)
                            .sum::<usize>()
                    })
                    .sum::<usize>();
                anyhow::ensure!(
                    self.speech_split.decoded.len() < MAX_RETAINED_JOBS
                        && retained_pcm
                            + output
                                .tracks
                                .iter()
                                .map(|t| t.samples.len() * 4)
                                .sum::<usize>()
                            <= MAX_RETAINED_BYTES,
                    "correction retained PCM bound"
                );
                input.samples = vec![]; // mixture is already archived; do not retain another PCM copy
                if let Some(call) = self.speech_split.job_calls.get(&key).cloned() {
                    let total = self.speech_split.call_cpu.entry(call.clone()).or_default();
                    *total += output.cpu_seconds;
                    self.index.evidence.event("speech_split_call_cpu",&call.0,&[],input.region.end_ns,serde_json::json!({"call_origin_ns":call.1,"cpu_helper_seconds":*total,"includes_model_loading":true,"excludes_live_asr_cpu":true,"call_active":self.speech_split.active.contains(&call)}))?;
                }
                self.index.evidence.event("speech_split_progress",&input.region.source,&[],input.region.start_ns,serde_json::json!({"job":key,"stage":"separated","cpu_seconds":output.cpu_seconds,"service_seconds":output.total_seconds,"separator_seconds":output.separation_seconds,"separator":heimdall_night::separator::MODEL_ID,"identity_model":speakers::embedding::MODEL_ID,"identities":output.tracks.iter().map(|t|&t.identity).collect::<Vec<_>>()}))?;
                self.speech_split.decoded.insert(
                    key,
                    Decoding {
                        input,
                        output,
                        words: [None, None],
                        admitted,
                    },
                );
                self.correction_tick()?;
            }
            Err(error) => {
                self.release_correction(&key);
                self.index.evidence.event(
                    "speech_split_failure",
                    &input.region.source,
                    &[],
                    input.region.start_ns,
                    serde_json::json!({"job":key,"stage":"cpu_helper","reason":error}),
                )?;
            }
        }
        Ok(())
    }
    pub(super) fn correction_transcript(
        &mut self,
        id: u64,
        result: Result<listen::Transcript, String>,
        decode_ms: f64,
        cpu_seconds: f64,
    ) -> Result<bool> {
        let Some((key, slot)) = self.speech_split.inflight.remove(&id) else {
            return Ok(false);
        };
        let Some(job) = self.speech_split.decoded.get_mut(&key) else {
            return Ok(true);
        };
        if let Some(call) = self.speech_split.job_calls.get(&key).cloned() {
            let asr = self
                .speech_split
                .call_asr_cpu
                .entry(call.clone())
                .or_default();
            *asr += cpu_seconds;
            self.index.evidence.event("speech_split_call_cpu",&call.0,&[],job.input.region.end_ns,serde_json::json!({"call_origin_ns":call.1,"cpu_helper_seconds":self.speech_split.call_cpu.get(&call).copied().unwrap_or(0.),"side_asr_process_cpu_delta_seconds":*asr,"asr_counter_includes_concurrent_host_work":true}))?;
        }
        match result {
            Ok(t) => {
                self.index.evidence.event("speech_split_asr_output",&job.input.region.source,&[],job.input.start_ns,
                    serde_json::json!({"job":key,"slot":slot,"model":"moondream/parakeet-ultra","lane":"correction","window_transcript":t,"actionable":false}))?;
                job.words[slot] = Some(t.words);
            }
            Err(error) => {
                let input = job.input.clone();
                self.speech_split.decoded.remove(&key);
                self.release_correction(&key);
                self.index.evidence.event(
                    "speech_split_failure",
                    &input.region.source,
                    &[],
                    input.region.start_ns,
                    serde_json::json!({"job":key,"stage":"side_asr","reason":error}),
                )?;
                return Ok(true);
            }
        }
        self.index.evidence.event(
            "speech_split_progress",
            &job.input.region.source,
            &[],
            job.input.region.start_ns,
            serde_json::json!({"job":key,"stage":"asr","slot":slot,"decode_ms":decode_ms}),
        )?;
        if job.words.iter().all(Option::is_some) {
            let job = self.speech_split.decoded.remove(&key).unwrap();
            let a = job.input.region.start_ns;
            let b = job.input.region.end_ns;
            let originals: Vec<_> = self
                .speech_split
                .finals
                .values()
                .filter(|f| f.source == job.input.region.source && f.start < b && f.end > a)
                .flat_map(|f| f.words.iter().cloned())
                .collect();
            let tracks = evidence_tracks(&job.words, &job.output, a, b, &originals);
            self.index.evidence.event("speech_split_completed",&job.input.region.source,&[],a,serde_json::json!({"job":key,"owned_start_ns":a,"owned_end_ns":b,"tracks":tracks,"lag_from_admission_seconds":job.admitted.elapsed().as_secs_f64(),"lag_from_overlap_end_seconds":if self.replay {None}else{Some(capture::host_now_ns().saturating_sub(b) as f64/1e9)},"model_recovery":false,"published":false,"policy":"attribution_only; diagnostic_gate_v1_unvalidated"}))?;
            self.release_correction(&key);
        }
        self.correction_tick()?;
        Ok(true)
    }
    pub(super) fn correction_tick(&mut self) -> Result<()> {
        self.correction_admit()?;
        if let Some(worker) = self.live_worker.as_ref() {
            for (key, job) in &self.speech_split.decoded {
                for slot in 0..2 {
                    if job.words[slot].is_some()
                        || self
                            .speech_split
                            .inflight
                            .values()
                            .any(|(k, s)| k == key && *s == slot)
                    {
                        continue;
                    }
                    let id = self.next_side_id;
                    self.next_side_id += 1;
                    if worker
                        .transcribe_correction(
                            id,
                            job.input.start_ns,
                            job.output.tracks[slot].samples.clone(),
                        )
                        .is_ok()
                    {
                        self.speech_split.inflight.insert(id, (key.clone(), slot));
                    }
                }
            }
        }
        if self.speech_split.outstanding == 0
            && self.speech_split.active.is_empty()
            && self.speech_split.pending.is_empty()
            && self.speech_split.decoded.is_empty()
            && self.speech_split.inflight.is_empty()
        {
            if let Some(runtime) = &self.speech_split.runtime {
                runtime.unload();
            }
        }
        Ok(())
    }
    pub(super) fn correction_worker_ended(&mut self) {
        // A replacement listen worker has no knowledge of requests held by the
        // old one. Retain decoded PCM and let the new owner's lane resubmit it.
        self.speech_split.inflight.clear();
    }
    pub(super) fn correction_end(&mut self, source: &str, origin: u64) {
        let _ = origin;
        self.speech_split.active.retain(|(s, _)| s != source);
    }
    pub(super) fn update_call_attribution(
        &mut self,
        source: &str,
        origin: u64,
        segments: Vec<speakers::diar::SpeakerSegment>,
    ) {
        let timeline = self
            .speech_split
            .timelines
            .entry((source.into(), origin))
            .or_default();
        if timeline.segments.len() == segments.len()
            && timeline.segments.iter().zip(&segments).all(|(a, b)| {
                a.speaker == b.speaker && a.start_time == b.start_time && a.end_time == b.end_time
            })
        {
            return;
        }
        timeline.segments = segments;
        self.invalidate_call_labels(source);
    }
    pub(super) fn call_final_ids(&self) -> HashSet<u64> {
        self.speech_split.finals.keys().copied().collect()
    }
    pub(super) fn remember_call_name(&mut self, track: &TrackId, name: &str) {
        self.speech_split.names.insert(track.clone(), name.into());
    }
    pub(super) fn rename_recovered(&mut self, old: &str, new: &str) -> Result<()> {
        for name in self.speech_split.names.values_mut() {
            if name == old {
                *name = new.into();
            }
        }
        self.speech_split
            .dirty
            .extend(self.speech_split.finals.keys());
        let sources: HashSet<_> = self
            .speech_split
            .finals
            .values()
            .map(|f| f.source.clone())
            .collect();
        for source in sources {
            self.project_call(&source)?;
        }
        Ok(())
    }
    pub(super) fn invalidate_call_labels(&mut self, source: &str) {
        for (id, f) in &self.speech_split.finals {
            if f.source == source {
                self.speech_split.dirty.insert(*id);
            }
        }
    }
    pub(super) fn call_final_runs(
        &self,
        id: &str,
        words: &[Word],
        source: &str,
    ) -> Option<Vec<Run>> {
        // A delayed final may belong to the previous call, even if this source
        // already has a new diarizer origin.
        let origin = id
            .rsplit(':')
            .next()
            .and_then(|id| id.parse::<u64>().ok())
            .and_then(|id| self.speech_split.finals.get(&id))
            .filter(|f| f.source == source)
            .map(|f| f.origin)
            .or_else(|| self.discord.get(source).map(|d| d.origin))?;
        Some(self.call_word_runs(words, source, origin))
    }
    pub(super) fn call_word_runs(&self, words: &[Word], source: &str, origin: u64) -> Vec<Run> {
        let mut names = self.speech_split.names.clone();
        names.extend(self.speaker_names.clone());
        let mut segments =
            if let Some(timeline) = self.speech_split.timelines.get(&(source.into(), origin)) {
                let mut segments = timeline.finalized.clone();
                segments.extend(timeline.segments.clone());
                segments
            } else {
                self.discord
                    .get(source)
                    .filter(|d| d.origin == origin)
                    .map(|d| d.segments.clone())
                    .unwrap_or_default()
            };
        if let (Some(start), Some(end)) = (
            words.iter().map(|w| w.start_ns).min(),
            words.iter().map(|w| w.end_ns).max(),
        ) {
            segments.retain(|s| {
                origin_ns(origin, s.end_time) > start && origin_ns(origin, s.start_time) < end
            });
        }
        word_runs(words, source, origin, &segments, &names)
    }
    pub(super) fn project_call(&mut self, source: &str) -> Result<()> {
        let ids: Vec<_> = self
            .speech_split
            .finals
            .iter()
            .filter(|(_, f)| f.source == source)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let final_ = &self.speech_split.finals[&id];
            if !final_.last.is_empty() && !self.speech_split.dirty.contains(&id) {
                let (speaker, text) = context_text(&final_.projected);
                let observation_id = format!(
                    "{}:{id}",
                    self.session.file_name().unwrap().to_string_lossy()
                );
                let mut context = self.tools.send_context.lock().unwrap();
                if context.revise_observation(&observation_id, &speaker, &text, final_.revision) {
                    self.tools.persist_send_context(&context)?;
                }
                continue;
            }
            if !self
                .speech_split
                .timelines
                .contains_key(&(source.into(), final_.origin)) {
                continue;
            }
            let runs = self.call_word_runs(&final_.words, source, final_.origin);
            let serialized = serde_json::to_string(&runs)?;
            self.speech_split.dirty.remove(&id);
            if serialized == final_.last {
                let (speaker, text) = context_text(&runs);
                let observation_id = format!(
                    "{}:{id}",
                    self.session.file_name().unwrap().to_string_lossy()
                );
                let mut context = self.tools.send_context.lock().unwrap();
                if context.revise_observation(&observation_id, &speaker, &text, final_.revision) {
                    self.tools.persist_send_context(&context)?;
                }
                continue;
            }
            let observation_id = format!(
                "{}:{id}",
                self.session.file_name().unwrap().to_string_lossy()
            );
            let revision = final_.revision + 1;
            if !self.index.revise_transcript(
                &observation_id,
                source,
                revision,
                &final_.words,
                &runs,
                false,
            )? {
                continue;
            }
            let label = self
                .sources
                .get(source)
                .map_or_else(|| display_name_from_id(source), display_name);
            let mut lines: Vec<_> = runs
                .iter()
                .map(|r| ui::TranscriptLine {
                    id,
                    label: Some(ui::Label {
                        source: label.clone(),
                        speaker: r.speaker.clone(),
                    }),
                    segments: vec![ui::Segment::Words(r.text())],
                    is_final: true,
                })
                .collect();
            if let Some(line) = lines.last_mut() {
                line.segments.extend(
                    final_
                        .notes
                        .iter()
                        .map(|s| ui::Segment::Note(s.trim_matches(['[', ']']).into())),
                );
            }
            self.ui(ui::UiUpdate::TranscriptRuns { id, lines });
            let (speaker, text) = context_text(&runs);
            let mut context = self.tools.send_context.lock().unwrap();
            context.revise_observation(&observation_id, &speaker, &text, revision);
            self.tools.persist_send_context(&context)?;
            drop(context);
            let final_ = self.speech_split.finals.get_mut(&id).unwrap();
            final_.last = serialized;
            final_.projected = runs;
            final_.revision = revision;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "speech_split_tests/tests.rs"]
mod tests;
