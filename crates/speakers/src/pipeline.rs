use crate::{
    diar::{DiarStream, SpeakerSegment},
    embedding::{VoiceEncoder, SAMPLE_RATE},
    naming::{NamingFlow, SpeakerEvent, TrackId},
};
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, VecDeque};

/// Exclude every overlapping region. Native segments have dataset-sensitive padded edges;
/// clean identity windows also remove 100 ms at edges, never fill gaps or mix tracks.
pub fn clean_intervals(
    segments: &[SpeakerSegment],
    speaker: i32,
    start: f64,
    end: f64,
) -> Vec<(f64, f64)> {
    let mut cuts = Vec::with_capacity(segments.len() * 2);
    for seg in segments {
        cuts.extend([seg.start_time, seg.end_time]);
    }
    cuts.sort_by(f64::total_cmp);
    cuts.dedup();
    let mut out: Vec<(f64, f64)> = Vec::new();
    for pair in cuts.windows(2) {
        let middle = (pair[0] + pair[1]) / 2.;
        let active: Vec<_> = segments
            .iter()
            .filter(|s| s.start_time <= middle && middle < s.end_time)
            .collect();
        if active.len() == 1 && active[0].speaker == speaker {
            if let Some(last) = out.last_mut().filter(|last| last.1 == pair[0]) {
                last.1 = pair[1];
            } else {
                out.push((pair[0], pair[1]));
            }
        }
    }
    // Trim actual speech/overlap edges before clipping the requested slice. A caller's
    // chunk boundary is not a speech boundary and must not discard another 100 ms.
    out.into_iter()
        .filter_map(|(a, b)| {
            let a = (a + 0.1).max(start);
            let b = (b - 0.1).min(end);
            (b > a).then_some((a, b))
        })
        .collect()
}

/// Native and process-isolated streams share the exact same naming pipeline.
pub trait Diarization {
    fn take_finalized_segments(&self) -> Result<Vec<SpeakerSegment>> { Ok(vec![]) }
    fn sample_rate(&self) -> u32;
    fn speakers(&self) -> usize;
    fn seconds_per_frame(&self) -> f64;
    fn frame_count(&self) -> i64;
    fn output_revision(&self) -> i64 { self.frame_count() }
    fn stable_frames(&self) -> i64 { self.frame_count() }
    fn frame_probabilities(&self, _since:i64) -> Result<Option<crate::diar::FrameProbabilities>> { Ok(None) }
    fn push_f32(&mut self, audio: &[f32]) -> Result<()>;
    fn finish(&mut self) -> Result<()>;
    fn segments(&self) -> Result<Vec<SpeakerSegment>>;
}
impl Diarization for DiarStream {
    fn take_finalized_segments(&self) -> Result<Vec<SpeakerSegment>> { self.take_finalized_segments() }
    fn frame_probabilities(&self,since:i64) -> Result<Option<crate::diar::FrameProbabilities>> { Ok(Some(self.probabilities_since(since)?)) }
    fn output_revision(&self) -> i64 { DiarStream::output_revision(self) }
    fn stable_frames(&self) -> i64 { DiarStream::stable_frames(self) }
    fn sample_rate(&self) -> u32 {
        self.sample_rate()
    }
    fn speakers(&self) -> usize {
        self.speakers
    }
    fn seconds_per_frame(&self) -> f64 {
        self.seconds_per_frame
    }
    fn frame_count(&self) -> i64 {
        self.frame_count()
    }
    fn push_f32(&mut self, audio: &[f32]) -> Result<()> {
        self.push_f32(audio)
    }
    fn finish(&mut self) -> Result<()> {
        self.finish()
    }
    fn segments(&self) -> Result<Vec<SpeakerSegment>> {
        self.segments()
    }
}

/// One source/call. Push exactly 16 kHz mono PCM with a continuous clock; start a new
/// session on a capture gap rather than silently joining speech across missing audio.
pub struct SpeakerPipeline<E: VoiceEncoder, S: Diarization = DiarStream> {
    pub naming: NamingFlow,
    stream: S,
    encoder: E,
    source: String,
    session: u64,
    origin: f64,
    audio: VecDeque<f32>,
    audio_start: usize,
    total: usize,
    last_frame: i64,
    last_model_revision: i64,
    stable_probability_frame: i64,
    last_probabilities: Option<crate::diar::FrameProbabilities>,
    probability_revision: u64,
    processed: BTreeMap<i32, f64>,
    quotes: BTreeMap<i32, String>,
    clean: BTreeMap<i32, Vec<f32>>,
    clean_start: BTreeMap<i32, f64>,
    clean_spans: BTreeMap<i32, Vec<(f64,f64)>>,
    evidence: Option<Box<dyn FnMut(serde_json::Value)->Result<()> + Send>>,
}
impl<E: VoiceEncoder, S: Diarization> SpeakerPipeline<E, S> {
    pub fn new(
        stream: S,
        encoder: E,
        naming: NamingFlow,
        source: String,
        session: u64,
        origin: f64,
    ) -> Result<Self> {
        ensure!(origin.is_finite(), "invalid source clock");
        ensure!(
            stream.sample_rate() == 16000,
            "identity pipeline needs 16-kHz PCM"
        );
        Ok(Self {
            naming,
            stream,
            encoder,
            source,
            session,
            origin,
            audio: VecDeque::new(),
            audio_start: 0,
            total: 0,
            last_frame: -1,
            last_model_revision: -1,
            stable_probability_frame: 0,
            last_probabilities: None,
            probability_revision: 0,
            processed: BTreeMap::new(),
            quotes: BTreeMap::new(),
            clean: BTreeMap::new(),
            clean_start: BTreeMap::new(),
            clean_spans: BTreeMap::new(),
            evidence: None,
        })
    }
    pub fn set_evidence_hook(&mut self, hook:Box<dyn FnMut(serde_json::Value)->Result<()>+Send>) {self.evidence=Some(hook);}
    pub fn observe_transcript(&mut self, speaker: i32, end: f64, text: &str) -> Vec<SpeakerEvent> {
        self.quotes.insert(speaker, text.into());
        self.naming
            .observe_transcript(self.track(speaker), end, text)
    }
    fn track(&self, speaker: i32) -> TrackId {
        TrackId {
            source: self.source.clone(),
            session: self.session,
            speaker,
        }
    }
    pub fn push(&mut self, audio: &[f32]) -> Result<Vec<SpeakerEvent>> {
        // Feeding chunk-sized pieces bounds native and Rust intake buffers even for a whole-file call.
        let mut events = vec![];
        for chunk in audio.chunks(320) {
            self.stream.push_f32(chunk)?;
            self.audio.extend(chunk.iter().copied());
            self.total += chunk.len();
            events.extend(self.process()?);
            let keep = 12 * SAMPLE_RATE;
            if self.audio.len() > keep {
                let discard = self.audio.len() - keep;
                self.audio.drain(..discard);
                self.audio_start += discard;
            }
        }
        Ok(events)
    }
    pub fn finish(&mut self) -> Result<Vec<SpeakerEvent>> {
        self.stream.finish()?;
        self.process()
    }
    pub fn into_naming(self) -> NamingFlow {
        self.naming
    }
    pub fn origin_seconds(&self) -> f64 { self.origin }
    pub fn stream_ref(&self) -> &S {
        &self.stream
    }
    pub fn segments(&self) -> Result<Vec<SpeakerSegment>> {
        self.stream.segments()
    }
    fn process(&mut self) -> Result<Vec<SpeakerEvent>> {
        let frame = self.stream.frame_count();
        let revision=self.stream.output_revision();
        if revision == self.last_model_revision { return Ok(vec![]); }
        self.last_model_revision=revision;
        self.last_frame = frame;
        if let Some(hook) = &mut self.evidence {
            if let Some(probabilities) = self.stream.frame_probabilities(self.stable_probability_frame)? {
                let mut changed=vec![];
                for (i,values) in probabilities.values.chunks(probabilities.speakers).enumerate() {
                    let frame=probabilities.first_frame+i as i64;
                    let prior=self.last_probabilities.as_ref().and_then(|p| {
                        let relative=frame-p.first_frame;
                        if relative<0 {return None;}
                        let offset=relative as usize*p.speakers;
                        p.values.get(offset..offset+p.speakers)
                    });
                    if prior.is_none_or(|p|p.iter().zip(values).any(|(a,b)|a.to_bits()!=b.to_bits())) {
                        changed.push(serde_json::json!({"frame":frame,"probabilities":values}));
                    }
                }
                let previous=self.probability_revision;self.probability_revision+=1;
                hook(serde_json::json!({"kind":"speaker_probabilities","model":"nemotron","source":self.source,"session":self.session,"revision":self.probability_revision,"origin_seconds":self.origin,"input":{"audio_id":self.source,"start_seconds":self.origin,"end_seconds":self.origin+self.total as f64/SAMPLE_RATE as f64,"rate":SAMPLE_RATE,"state":"previous invocation source/session stream state"},"output":{"first_frame":probabilities.first_frame,"frame_count":probabilities.frame_count,"speakers":probabilities.speakers,"seconds_per_frame":probabilities.seconds_per_frame,"previous_revision":previous,"changed_frames":changed}}))?;
                self.stable_probability_frame=self.stream.stable_frames();
                self.last_probabilities=Some(probabilities);
            }
        }
        if let Some(hook) = &mut self.evidence {
            let finalized = self.stream.take_finalized_segments()?;
            if !finalized.is_empty() {
                hook(serde_json::json!({"kind":"speaker_segments_finalized","source":self.source,"session":self.session,"origin_seconds":self.origin,"segments":finalized}))?;
            }
        }
        let segments = self.stream.segments()?;
        // Native labelled frames lag capture while awaiting context. Never enrol its unlabelled tail.
        let now = (frame as f64 * self.stream.seconds_per_frame())
            .min(self.total as f64 / SAMPLE_RATE as f64);
        let mut events = vec![];
        for speaker in 1..=self.stream.speakers() as i32 {
            let track = self.track(speaker);
            if self.naming.labelled(&track) {
                continue;
            }
            let from = self
                .processed
                .get(&speaker)
                .copied()
                .unwrap_or(0.)
                .max(self.audio_start as f64 / SAMPLE_RATE as f64);
            let target = 2 * crate::embedding::WINDOW_SAMPLES; // two independent six-second enrollment windows.
            for (start, end) in clean_intervals(&segments, speaker, from, now) {
                let a = (start * SAMPLE_RATE as f64).ceil() as usize;
                let b = ((end * SAMPLE_RATE as f64).floor() as usize).min(self.total);
                if b <= a || a < self.audio_start {
                    continue;
                }
                self.clean_start.entry(speaker).or_insert(start);
                let clean = self.clean.entry(speaker).or_default();
                let n = (b - a).min(target - clean.len());
                self.clean_spans.entry(speaker).or_default().push((self.origin+a as f64/SAMPLE_RATE as f64,self.origin+(a+n) as f64/SAMPLE_RATE as f64));
                clean.extend(
                    self.audio
                        .range(a - self.audio_start..a - self.audio_start + n)
                        .copied(),
                );
                if clean.len() == target {
                    break;
                }
            }
            self.processed.insert(speaker, (now - 0.1).max(from));
            if self.clean.get(&speaker).is_some_and(|v| v.len() == target) {
                let clean = self.clean.remove(&speaker).unwrap();
                let spans=self.clean_spans.remove(&speaker).unwrap_or_default();
                let mut embeddings=vec![];
                for (window_index,window) in clean.as_chunks::<{ crate::embedding::WINDOW_SAMPLES }>().0.iter().enumerate() {
                    let embedding=self.encoder.embed(window)?;
                    if let Some(hook)=&mut self.evidence {
                        hook(serde_json::json!({"kind":"speaker_embeddings","source":self.source,"session":self.session,"track":track,"rate":SAMPLE_RATE,"input_spans_seconds":spans,"input_slice_samples":[window_index*crate::embedding::WINDOW_SAMPLES,(window_index+1)*crate::embedding::WINDOW_SAMPLES],"samples":window.len(),"embeddings":[embedding],"model":crate::embedding::MODEL_ID}))?;
                    }
                    embeddings.push(embedding);
                }
                events.extend(self.naming.clean_voice_samples(
                    track.clone(),
                    self.origin + self.clean_start.remove(&speaker).unwrap_or(from),
                    embeddings,
                    clean.len() as f64 / SAMPLE_RATE as f64,
                    self.quotes.get(&speaker).map(String::as_str).unwrap_or(""),
                ));
            }
        }
        Ok(events)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diar_revisions_with_same_frame_count_retain_changed_tail_without_recopying_history() {
        struct Stream { revision:i64 }
        impl Diarization for Stream {
            fn sample_rate(&self)->u32{16000} fn speakers(&self)->usize{2}
            fn seconds_per_frame(&self)->f64{0.08} fn frame_count(&self)->i64{2}
            fn output_revision(&self)->i64{self.revision}
            fn stable_frames(&self)->i64{1}
            fn push_f32(&mut self,_:&[f32])->Result<()>{self.revision+=1;Ok(())}
            fn finish(&mut self)->Result<()>{self.revision+=1;Ok(())}
            fn segments(&self)->Result<Vec<SpeakerSegment>>{Ok(vec![])}
            fn frame_probabilities(&self,since:i64)->Result<Option<crate::diar::FrameProbabilities>> {
                let mut values=vec![0.1,0.2,self.revision as f32/10.,0.4];
                values.drain(..since as usize*2);
                Ok(Some(crate::diar::FrameProbabilities{first_frame:since,frame_count:2,speakers:2,seconds_per_frame:0.08,values}))
            }
        }
        struct Encoder;
        impl VoiceEncoder for Encoder {fn embed(&mut self,_:&[f32])->Result<crate::embedding::Embedding>{panic!("no speech segments")}}
        let saved=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));let records=saved.clone();
        let mut pipeline=SpeakerPipeline::new(Stream{revision:0},Encoder,NamingFlow::new(Default::default()),"discord".into(),1,1.).unwrap();
        pipeline.set_evidence_hook(Box::new(move|p|{records.lock().unwrap().push(p);Ok(())}));
        pipeline.push(&[0.;320]).unwrap();pipeline.push(&[0.;320]).unwrap();pipeline.finish().unwrap();
        let records=saved.lock().unwrap();assert_eq!(records.len(),3);
        assert_eq!(records[0]["output"]["changed_frames"].as_array().unwrap().len(),2);
        for r in &records[1..] {
            assert_eq!(r["output"]["changed_frames"].as_array().unwrap().len(),1);
            assert_eq!(r["output"]["changed_frames"][0]["frame"],1);
        }
        assert_eq!(records[2]["output"]["previous_revision"],2);
    }
    #[test]
    fn overlap_never_enrolls_mixed_speech() {
        let segs = vec![
            SpeakerSegment {
                start_time: 0.,
                end_time: 10.,
                speaker: 1,
            },
            SpeakerSegment {
                start_time: 3.,
                end_time: 7.,
                speaker: 2,
            },
        ];
        assert_eq!(
            clean_intervals(&segs, 1, 0., 10.),
            vec![(0.1, 2.9), (7.1, 9.9)]
        );
        assert!(clean_intervals(&segs, 2, 0., 10.).is_empty());
    }
    #[test]
    fn chunk_clipping_does_not_trim_a_speech_edge_twice() {
        let segs = vec![SpeakerSegment {
            start_time: 0.,
            end_time: 10.,
            speaker: 1,
        }];
        assert_eq!(clean_intervals(&segs, 1, 2., 3.), vec![(2., 3.)]);
    }
}
