#[cfg(target_os = "macos")]
use crate::coreml::{Array, Model};
use anyhow::{ensure, Result};
#[cfg(target_os = "macos")]
use objc2_core_ml::{MLComputeUnits, MLMultiArrayDataType};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub confidence: f32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub words: Vec<Word>,
}
impl Transcript {
    pub fn from_words(words: Vec<Word>) -> Self {
        Self {
            text: words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            words,
        }
    }
}
#[derive(Debug, Clone)]
pub struct Token {
    pub id: usize,
    pub frame: usize,
    pub duration: usize,
    pub probability: f32,
}
/// SentencePiece word boundaries, with punctuation attached to its word.
pub fn words_from_tokens(
    tokens: &[Token],
    vocab: &[String],
    start_ns: u64,
    audio_len: usize,
) -> Vec<Word> {
    words_from_tokens_with_vocabulary(tokens, vocab, start_ns, audio_len, None)
}
fn words_from_tokens_with_vocabulary(
    tokens: &[Token],
    vocab: &[String],
    start_ns: u64,
    audio_len: usize,
    vocabulary: Option<&crate::vocabulary::Vocabulary>,
) -> Vec<Word> {
    let spans = vocabulary
        .map(|v| crate::vocabulary::completed_spans(v, vocab, tokens))
        .unwrap_or_default();
    let end_ns = start_ns + audio_len as u64 * 62500;
    let mut words: Vec<Word> = vec![];
    let mut pending_boundary = false;
    for (i, t) in tokens.iter().enumerate() {
        if spans
            .iter()
            .any(|(first, last, _)| i > *first && i <= *last)
        {
            continue;
        }
        let completed = spans.iter().find(|(first, _, _)| *first == i);
        let canonical_piece = completed.map(|(_, _, word)| format!(" {word}"));
        let Some(piece) = canonical_piece.as_ref().or_else(|| vocab.get(t.id)) else {
            continue;
        };
        if piece.starts_with('<') && piece.ends_with('>') {
            continue;
        }
        let lexicon_boundary = words.last().is_some_and(|word| {
            piece.chars().next().is_some_and(char::is_alphabetic)
                && vocabulary.is_some_and(|v| {
                    v.words.iter().any(|name| {
                        name.chars().count() >= 4 && name.to_lowercase() == word.text.to_lowercase()
                    })
                })
                && !is_vocabulary_suffix(&tokens[i..], vocab)
        });
        let boundary = pending_boundary
            || piece.starts_with('▁')
            || piece.starts_with(' ')
            || lexicon_boundary;
        let text = piece.replace('▁', " ");
        let text = text.trim();
        if text.is_empty() {
            pending_boundary = boundary;
            continue;
        }
        pending_boundary = false;
        // Match FluidAudio's one-frame emission-delay correction.
        let frame = t.frame.saturating_sub(1);
        let begin = start_ns + frame as u64 * 80_000_000;
        let length = if t.duration > 0 {
            t.duration.max(1)
        } else {
            tokens
                .get(i + 1)
                .map(|n| n.frame.saturating_sub(1).saturating_sub(frame).max(1))
                .unwrap_or(1)
        };
        let mut end = (begin + length as u64 * 80_000_000).min(end_ns);
        let mut confidence = t.probability;
        if let Some((_, last, _)) = completed {
            let tail = &tokens[*last];
            let tail_frame = tail.frame.saturating_sub(1);
            let tail_length = if tail.duration > 0 {
                tail.duration
            } else {
                tokens
                    .get(last + 1)
                    .map(|n| n.frame.saturating_sub(1).saturating_sub(tail_frame).max(1))
                    .unwrap_or(1)
            };
            end = (start_ns + (tail_frame + tail_length) as u64 * 80_000_000).min(end_ns);
            confidence = tokens[i..=*last]
                .iter()
                .map(|t| t.probability)
                .fold(confidence, f32::min);
        }
        if boundary || words.is_empty() {
            words.push(Word {
                text: text.into(),
                start_ns: begin.min(end_ns),
                end_ns: end,
                confidence,
            });
        } else {
            let word = words.last_mut().unwrap();
            word.text.push_str(text);
            word.end_ns = word.end_ns.max(end);
            word.confidence = word.confidence.min(confidence);
        }
    }
    // A completed phrase ("T3 Code") is one decoder span; consumers get one word per phrase
    // word, the span's time shared by character count.
    if words.iter().any(|w| w.text.contains(' ')) {
        words = words
            .into_iter()
            .flat_map(|w| {
                let parts: Vec<&str> = w.text.split(' ').filter(|p| !p.is_empty()).collect();
                let total = parts.iter().map(|p| p.chars().count() as u64).sum::<u64>().max(1);
                let span = w.end_ns.saturating_sub(w.start_ns);
                let mut seen = 0;
                parts
                    .into_iter()
                    .map(|p| {
                        let start = w.start_ns + span * seen / total;
                        seen += p.chars().count() as u64;
                        Word {
                            text: p.into(),
                            start_ns: start,
                            end_ns: w.start_ns + span * seen / total,
                            confidence: w.confidence,
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    // Canonical capitalization of an exactly decoded name; no fuzzy text repair.
    if let Some(v) = vocabulary {
        for word in &mut words {
            let end = word
                .text
                .find(|c: char| !c.is_alphabetic())
                .unwrap_or(word.text.len());
            if let Some(name) = v
                .words
                .iter()
                .find(|name| name.to_lowercase() == word.text[..end].to_lowercase())
            {
                word.text.replace_range(..end, name);
            }
        }
    }
    words
}
/// Examine the whole joined tail, rather than its first token: `e` + `s` is a
/// suffix, but `s` + `ystem` is another word. SentencePiece boundaries still win.
fn is_vocabulary_suffix(tokens: &[Token], vocab: &[String]) -> bool {
    let mut tail = String::new();
    'pieces: for token in tokens {
        let Some(piece) = vocab.get(token.id) else {
            break;
        };
        for c in piece.chars() {
            if c.is_alphabetic() || matches!(c, '\'' | '’') {
                tail.extend(c.to_lowercase());
                if tail.len() > 4 {
                    return false;
                }
            } else {
                break 'pieces;
            }
        }
    }
    matches!(
        tail.as_str(),
        "s" | "es" | "ed" | "d" | "ing" | "er" | "ers" | "est" | "ly" | "'s" | "’s"
    )
}
/// Merge timed overlap by matching the same words near the seam. Repeated words
/// far apart are preserved; new windows own the revisable overlap.
pub fn reconcile(old: &[Word], new: &[Word], window_start: u64) -> Vec<Word> {
    // Same-origin provisional decode revises the entire window. Matching a
    // later unchanged word must not freeze an earlier wrong word.
    if old.first().is_none_or(|w| w.start_ns >= window_start) {
        return new.to_vec();
    }
    let mut cut = old.partition_point(|w| w.end_ns <= window_start);
    // Alignment can put a seam word just before the new acoustic origin.
    // Search the retained margin too; otherwise "students" is appended twice.
    let search_start = old.partition_point(|w| w.end_ns.saturating_add(640_000_000) < window_start);
    let mut skip = 0;
    for (j, n) in new.iter().take(12).enumerate() {
        if let Some(i) = old
            .iter()
            .enumerate()
            .skip(search_start)
            .find_map(|(i, o)| {
                (o.text
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
                    == n.text
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase()
                    && o.start_ns.abs_diff(n.start_ns) <= 640_000_000)
                    .then_some(i)
            })
        {
            cut = i;
            skip = j;
            break;
        }
    }
    let mut out = old[..cut].to_vec();
    out.extend_from_slice(&new[skip..]);
    out
}

pub trait Recognizer {
    fn transcribe(&mut self, audio: &[f32], start_ns: u64) -> Result<Transcript>;
    fn transcribe_batch(&mut self, inputs: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
        inputs
            .iter()
            .map(|(a, ns)| self.transcribe(a, *ns))
            .collect()
    }
}
impl<R: Recognizer + ?Sized> Recognizer for &mut R {
    fn transcribe(&mut self, a: &[f32], ns: u64) -> Result<Transcript> {
        (**self).transcribe(a, ns)
    }
    fn transcribe_batch(&mut self, inputs: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
        (**self).transcribe_batch(inputs)
    }
}
#[cfg(target_os = "macos")]
struct ParakeetModel {
    encoder_calls: u64,
    pre: Model,
    encoder: Option<Model>,
    metal_encoder: Option<crate::metal::MetalEncoder>,
    decoder: Option<Model>,
    joint: Option<Model>,
    metal_decode: Option<crate::metal::MetalDecode>,
    vocab: Vec<String>,
    audio: Array,
    audio_length: Array,
    target: Array,
    target_length: Array,
    enc_step: Array,
    dec_step: Array,
    vocabulary: crate::vocabulary::Vocabulary,
}
#[cfg(target_os = "macos")]
impl ParakeetModel {
    pub fn load_backend(
        path: &Path,
        backend: &str,
        vocabulary: &crate::vocabulary::Vocabulary,
    ) -> Result<Self> {
        ensure!(
            matches!(backend, "coreml" | "metal" | "cpu"),
            "Unknown Parakeet backend {backend}"
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("parakeet_v3_vocab.json"))?)?;
        let mut vocab = vec![String::new(); 8192];
        if let Some(dict) = raw.as_object() {
            for (id, text) in dict {
                let id: usize = id.parse()?;
                if id < 8192 {
                    vocab[id] = text.as_str().unwrap_or("").into();
                }
            }
        } else if let Some(arr) = raw.as_array() {
            vocab = arr
                .iter()
                .map(|v| v.as_str().unwrap_or("").into())
                .collect();
        }
        ensure!(
            vocab.len() == 8192 && vocab.iter().filter(|s| !s.is_empty()).count() > 8000,
            "Unexpected Ultra vocabulary"
        );
        let f = MLMultiArrayDataType::Float32;
        let i = MLMultiArrayDataType::Int32;
        let target_length = Array::zeros(&[1], i)?;
        target_length.set(0, 1.0);
        let mut metal_decode = if backend == "metal" {
            Some(crate::metal::MetalDecode::load(path)?)
        } else {
            None
        };
        if let Some(gpu) = &mut metal_decode {
            gpu.set_vocabulary(vocabulary, &vocab)?;
        }
        Ok(Self {
            encoder_calls: 0,
            pre: Model::load(&path.join("Preprocessor.mlmodelc"), MLComputeUnits::CPUOnly)?,
            encoder: if backend != "metal" {
                Some(Model::load(
                    &path.join("Encoder.mlmodelc"),
                    if backend=="cpu" {MLComputeUnits::CPUOnly} else {MLComputeUnits::CPUAndNeuralEngine},
                )?)
            } else {
                None
            },
            metal_encoder: if backend == "metal" {
                Some(crate::metal::MetalEncoder::load(path)?)
            } else {
                None
            },
            decoder: if backend != "metal" {
                Some(Model::load(
                    &path.join("Decoder.mlmodelc"),
                    if backend=="cpu" {MLComputeUnits::CPUOnly} else {MLComputeUnits::CPUAndNeuralEngine},
                )?)
            } else {
                None
            },
            joint: if backend != "metal" {
                Some(Model::load(
                    &path.join("JointDecisionv3.mlmodelc"),
                    if backend=="cpu" {MLComputeUnits::CPUOnly} else {MLComputeUnits::CPUAndNeuralEngine},
                )?)
            } else {
                None
            },
            metal_decode,
            vocab,
            audio: Array::zeros(&[1, 240000], f)?,
            audio_length: Array::zeros(&[1], i)?,
            target: Array::zeros(&[1, 1], i)?,
            target_length,
            enc_step: Array::zeros(&[1, 1024, 1], f)?,
            dec_step: Array::zeros(&[1, 640, 1], f)?,
            vocabulary: vocabulary.clone(),
        })
    }
    pub fn backend(&self) -> &'static str {
        if self.metal_encoder.is_some() {
            "metal"
        } else {
            "coreml"
        }
    }
    /// Completed encoder invocations, for bounded resident-path probes.
    pub fn encoder_calls(&self) -> u64 {
        self.encoder_calls
    }
    fn predict_token(&mut self, token: usize, h: &Array, c: &Array) -> Result<(Array, Array)> {
        if let Some(gpu) = &mut self.metal_decode {
            let (projection, hv, cv) = gpu.predict(token, &h.values()?, &c.values()?)?;
            self.dec_step.copy_f32(&projection)?;
            let h = Array::zeros(&[2, 1, 640], MLMultiArrayDataType::Float32)?;
            let c = Array::zeros(&[2, 1, 640], MLMultiArrayDataType::Float32)?;
            h.copy_f32(&hv)?;
            c.copy_f32(&cv)?;
            return Ok((h, c));
        }
        self.target.set(0, token as f32);
        let out = self
            .decoder
            .as_mut()
            .expect("selected Core ML decoder")
            .predict(&[
                ("targets", &self.target),
                ("target_length", &self.target_length),
                ("h_in", h),
                ("c_in", c),
            ])?;
        let projection = out.array("decoder")?;
        let shape = projection.shape();
        let axis = shape
            .iter()
            .position(|&v| v == 640)
            .ok_or_else(|| anyhow::anyhow!("Invalid decoder projection {shape:?}"))?;
        self.dec_step
            .copy_f32(&projection.axis_values(axis, &vec![0; shape.len()])?)?;
        Ok((out.array("h_out")?, out.array("c_out")?))
    }
    fn window(&mut self, audio: &[f32], start_ns: u64) -> Result<Transcript> {
        ensure!(audio.len() <= 240000, "Ultra window exceeds 15 seconds");
        let mut padded = vec![0_f32; 240000];
        padded[..audio.len()].copy_from_slice(audio);
        self.audio.copy_f32(&padded)?;
        self.audio_length.set(0, audio.len() as f32);
        let profile_start = std::time::Instant::now();
        let mel = self.pre.predict(&[
            ("audio_signal", &self.audio),
            ("audio_length", &self.audio_length),
        ])?;
        let mel_array = mel.array("mel")?;
        let length = mel.array("mel_length")?;
        let pre_seconds = profile_start.elapsed().as_secs_f64();
        let encoder_start = std::time::Instant::now();
        let (enc, len) = if let Some(gpu) = &mut self.metal_encoder {
            let (v, n) = gpu.encode(&mel_array.values()?, length.get(0) as usize)?;
            let a = Array::zeros(&[1, 1024, 188], MLMultiArrayDataType::Float32)?;
            a.copy_f32(&v)?;
            (a, n)
        } else {
            let encoded = self
                .encoder
                .as_mut()
                .expect("selected Core ML encoder")
                .predict(&[("mel", &mel_array), ("mel_length", &length)])?;
            (
                encoded.array("encoder")?,
                encoded.array("encoder_length")?.get(0) as usize,
            )
        };
        self.encoder_calls = self.encoder_calls.saturating_add(1);
        ensure!(
            enc.shape() == [1, 1024, 188] && len <= 188,
            "Unexpected encoder contract"
        );
        let encoder_seconds = encoder_start.elapsed().as_secs_f64();
        let decoder_start = std::time::Instant::now();
        let result = self.decode_encoded(audio, start_ns, &enc, len)?;
        if std::env::var_os("HEIMDALL_PARAKEET_PROFILE").is_some() {
            eprintln!(
                "{}",
                serde_json::json!({"parakeet_profile":true,"backend":self.backend(),"pre_seconds":pre_seconds,"encoder_seconds":encoder_seconds,"tdt_seconds":decoder_start.elapsed().as_secs_f64()})
            );
        }
        Ok(result)
    }
    fn decode_encoded(
        &mut self,
        audio: &[f32],
        start_ns: u64,
        enc: &Array,
        len: usize,
    ) -> Result<Transcript> {
        let frames = len.min(audio.len().div_ceil(1280));
        if let Some(gpu) = &mut self.metal_decode {
            gpu.reset_vocabulary();
            if std::env::var("HEIMDALL_METAL_TDT").as_deref() != Ok("step") {
                let tokens = gpu.decode(&enc.values()?, frames)?;
                return Ok(Transcript::from_words(words_from_tokens_with_vocabulary(
                    &tokens,
                    &self.vocab,
                    start_ns,
                    audio.len(),
                    Some(&self.vocabulary),
                )));
            }
        }
        let h = Array::zeros(&[2, 1, 640], MLMultiArrayDataType::Float32)?;
        let c = Array::zeros(&[2, 1, 640], MLMultiArrayDataType::Float32)?;
        let (mut h, mut c) = self.predict_token(8192, &h, &c)?;
        let mut t = 0;
        let mut tokens = vec![];
        let mut same_frame = 0;
        while t < frames {
            self.enc_step.copy_f32(&enc.axis_values(1, &[0, 0, t])?)?;
            let (token, duration, probability) = if let Some(gpu) = &mut self.metal_decode {
                gpu.decision(&self.enc_step.values()?, &self.dec_step.values()?)?
            } else {
                let out = self
                    .joint
                    .as_mut()
                    .expect("selected Core ML joint")
                    .predict(&[
                        ("encoder_step", &self.enc_step),
                        ("decoder_step", &self.dec_step),
                    ])?;
                (
                    out.array("token_id")?.get(0) as usize,
                    out.array("duration")?.get(0) as usize,
                    out.array("token_prob")?.get(0),
                )
            };
            ensure!(
                token <= 8192 && duration <= 4 && probability.is_finite(),
                "Invalid TDT decision"
            );
            if token != 8192 {
                tokens.push(Token {
                    id: token,
                    frame: t,
                    duration,
                    probability,
                });
                (h, c) = self.predict_token(token, &h, &c)?;
                same_frame += 1;
                ensure!(tokens.len() < 2048, "TDT token guard reached");
            }
            let advance = if token == 8192 || same_frame >= 10 {
                duration.max(1)
            } else {
                duration
            };
            if advance > 0 {
                t += advance;
                same_frame = 0;
            }
        }
        Ok(Transcript::from_words(words_from_tokens_with_vocabulary(
            &tokens,
            &self.vocab,
            start_ns,
            audio.len(),
            Some(&self.vocabulary),
        )))
    }
}
#[cfg(target_os = "macos")]
impl Recognizer for ParakeetModel {
    fn transcribe_batch(&mut self, inputs: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
        if self.metal_encoder.is_none()
            || inputs.len() < 2
            || inputs.len() > 32
            || inputs
                .iter()
                .any(|(a, _)| a.len() > 240000 || !a.iter().all(|v| v.is_finite()))
        {
            return inputs
                .iter()
                .map(|(a, ns)| self.transcribe(a, *ns))
                .collect();
        }
        let result = objc2::rc::autoreleasepool(|_| -> Result<Vec<Result<Transcript>>> {
            let mut mels = Vec::with_capacity(inputs.len());
            let mut lengths = Vec::with_capacity(inputs.len());
            for (audio, _) in inputs {
                let mut padded = vec![0.; 240000];
                padded[..audio.len()].copy_from_slice(audio);
                self.audio.copy_f32(&padded)?;
                self.audio_length.set(0, audio.len() as f32);
                let f = self.pre.predict(&[
                    ("audio_signal", &self.audio),
                    ("audio_length", &self.audio_length),
                ])?;
                mels.push(f.array("mel")?.values()?);
                lengths.push(f.array("mel_length")?.get(0) as usize);
            }
            let encoded = self
                .metal_encoder
                .as_mut()
                .unwrap()
                .encode_batch(&mels, &lengths)?;
            self.encoder_calls = self.encoder_calls.saturating_add(1);
            Ok(inputs
                .iter()
                .zip(encoded)
                .map(|((audio, ns), (v, len))| {
                    let a = Array::zeros(&[1, 1024, 188], MLMultiArrayDataType::Float32)?;
                    a.copy_f32(&v)?;
                    self.decode_encoded(audio, *ns, &a, len)
                })
                .collect())
        });
        match result {
            Ok(v) => v,
            Err(e) => {
                let message = e.to_string();
                inputs
                    .iter()
                    .map(|_| {
                        Err(
                            if e.downcast_ref::<crate::gpu_recovery::GpuError>().is_some() {
                                crate::gpu_recovery::GpuError(message.clone()).into()
                            } else {
                                anyhow::anyhow!(message.clone())
                            },
                        )
                    })
                    .collect()
            }
        }
    }
    fn transcribe(&mut self, audio: &[f32], start_ns: u64) -> Result<Transcript> {
        ensure!(audio.iter().all(|v| v.is_finite()), "Non-finite audio");
        let mut words = vec![];
        let mut offset = 0;
        while offset < audio.len() {
            let end = (offset + 240000).min(audio.len());
            let ns = start_ns + offset as u64 * 62500;
            let result = objc2::rc::autoreleasepool(|_| self.window(&audio[offset..end], ns))?;
            words = reconcile(&words, &result.words, ns);
            if end == audio.len() {
                break;
            }
            offset += 208000; // 13 s progress / 2 s overlap.
        }
        Ok(Transcript::from_words(words))
    }
}
#[cfg(target_os = "macos")]
pub struct Parakeet {
    fallback_backend: &'static str,
    state: crate::gpu_recovery::Recovery<ParakeetModel>,
    path: std::path::PathBuf,
    encoder_calls: u64,
    vocabulary: crate::vocabulary::Vocabulary,
}
#[cfg(target_os = "macos")]
impl Parakeet {
    pub fn load(path: &Path) -> Result<Self> {
        let backend = std::env::var("HEIMDALL_PARAKEET_BACKEND").unwrap_or_else(|_| "metal".into());
        Self::load_backend(path, &backend)
    }
    pub fn load_backend(path: &Path, backend: &str) -> Result<Self> {
        ensure!(
            matches!(backend, "coreml" | "metal" | "cpu"),
            "Unknown Parakeet backend {backend}"
        );
        let vocabulary = crate::vocabulary::Vocabulary::from_env()?;
        let fallback_backend=if backend=="cpu" {"cpu"} else {"coreml"};
        Ok(Self {
            fallback_backend,
            state: crate::gpu_recovery::Recovery::load(backend == "metal", |gpu| {
                ParakeetModel::load_backend(path, if gpu { "metal" } else { fallback_backend }, &vocabulary)
            })?,
            path: path.to_owned(),
            encoder_calls: 0,
            vocabulary,
        })
    }
    /// Applies to the next decode, retaining the configuration across GPU recovery.
    pub fn set_vocabulary(&mut self, vocabulary: crate::vocabulary::Vocabulary) -> Result<()> {
        vocabulary.validate()?;
        self.state.run(
            |gpu| {
                ParakeetModel::load_backend(
                    &self.path,
                    if gpu { "metal" } else { self.fallback_backend },
                    &self.vocabulary,
                )
            },
            |model| {
                if let Some(gpu) = &mut model.metal_decode {
                    gpu.set_vocabulary(&vocabulary, &model.vocab)?;
                }
                model.vocabulary = vocabulary.clone();
                Ok(())
            },
        )?;
        self.vocabulary = vocabulary;
        Ok(())
    }
    pub fn backend(&self) -> &'static str {
        if self.state.is_gpu() {
            "metal"
        } else {
            self.fallback_backend
        }
    }
    pub fn encoder_calls(&self) -> u64 {
        self.encoder_calls
    }
}
#[cfg(target_os = "macos")]
impl Recognizer for Parakeet {
    fn transcribe(&mut self, audio: &[f32], start_ns: u64) -> Result<Transcript> {
        ensure!(audio.iter().all(|v| v.is_finite()), "Non-finite audio");
        let (output, calls) = self.state.run(
            |gpu| {
                ParakeetModel::load_backend(
                    &self.path,
                    if gpu { "metal" } else { self.fallback_backend },
                    &self.vocabulary,
                )
            },
            |model| {
                let before = model.encoder_calls();
                let output = model.transcribe(audio, start_ns)?;
                Ok((output, model.encoder_calls() - before))
            },
        )?;
        self.encoder_calls += calls;
        Ok(output)
    }
    fn transcribe_batch(&mut self, inputs: &[(&[f32], u64)]) -> Vec<Result<Transcript>> {
        // Validate individually before running the transactional GPU batch.
        if inputs.iter().any(|(a, _)| !a.iter().all(|v| v.is_finite())) {
            return inputs
                .iter()
                .map(|(a, ns)| self.transcribe(a, *ns))
                .collect();
        }
        let result = self.state.run(
            |gpu| {
                ParakeetModel::load_backend(
                    &self.path,
                    if gpu { "metal" } else { self.fallback_backend },
                    &self.vocabulary,
                )
            },
            |model| {
                let before = model.encoder_calls();
                let outputs = model
                    .transcribe_batch(inputs)
                    .into_iter()
                    .collect::<Result<Vec<_>>>()?;
                Ok((outputs, model.encoder_calls() - before))
            },
        );
        match result {
            Ok((outputs, calls)) => {
                self.encoder_calls += calls;
                outputs.into_iter().map(Ok).collect()
            }
            Err(error) => {
                let message = format!("{error:#}");
                inputs
                    .iter()
                    .map(|_| Err(anyhow::anyhow!(message.clone())))
                    .collect()
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acoustic_entry_formatting_requires_completion_and_retains_evidence() {
        let vocab = vec![
            " Hi".into(),
            " M".into(),
            "dal".into(),
            "l".into(),
            "bug".into(),
            ",".into(),
        ];
        let tokens: Vec<_> = (0..6)
            .map(|id| Token {
                id,
                frame: id + 1,
                duration: 1,
                probability: if id == 2 { 0.5 } else { 0.9 },
            })
            .collect();
        let config = crate::vocabulary::Vocabulary {
            words: vec!["Heimdall".into()],
            ..Default::default()
        };
        let words = words_from_tokens_with_vocabulary(&tokens, &vocab, 0, 16000, Some(&config));
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            vec!["Heimdall", "bug,"]
        );
        assert_eq!(
            (words[0].start_ns, words[0].end_ns, words[0].confidence),
            (0, 320_000_000, 0.5)
        );
        let incomplete =
            words_from_tokens_with_vocabulary(&tokens[..3], &vocab, 0, 16000, Some(&config));
        assert_eq!(
            incomplete
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Hi", "Mdal"]
        );
        let short_vocab = vec![" Pi".into(), "ck".into()];
        let short = words_from_tokens_with_vocabulary(
            &tokens[..2],
            &short_vocab,
            0,
            16000,
            Some(&crate::vocabulary::Vocabulary {
                words: vec!["Pi".into()],
                ..config
            }),
        );
        assert_eq!(short[0].text, "Pick");
    }
    #[test]
    fn vocabulary_names_own_their_token_boundary_but_keep_punctuation_and_timing() {
        let vocab = vec![" heim".into(), "dall".into(), "bug".into(), ".".into()];
        let tokens = (0..4)
            .map(|id| Token {
                id,
                frame: id * 2 + 1,
                duration: 1,
                probability: 0.9,
            })
            .collect::<Vec<_>>();
        let config = crate::vocabulary::Vocabulary::default();
        let words = words_from_tokens_with_vocabulary(&tokens, &vocab, 0, 16000, Some(&config));
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["Heimdall", "bug."]
        );
        assert_eq!(words[1].start_ns, 320_000_000);
        assert_eq!(words[0].confidence, 0.9);
        assert_eq!(
            words_from_tokens(&tokens, &vocab, 0, 16000)[0].text,
            "heimdallbug."
        );
    }
    #[test]
    fn vocabulary_suffixes_stay_attached_across_token_pieces() {
        for (pieces, name, expected) in [
            (vec![" har", "ness", "es"], "harness", "harnesses"),
            (vec![" har", "ness", "e", "s", ","], "harness", "harnesses,"),
            (
                vec![" op", "tim", "iz", "ation", "s"],
                "optimization",
                "optimizations",
            ),
            (vec![" har", "ness", "ed"], "harness", "harnessed"),
            (vec![" har", "ness", "i", "ng"], "harness", "harnessing"),
            (vec![" heim", "dall", "'", "s"], "Heimdall", "Heimdall's"),
            (vec![" heim", "dall", "’", "s"], "Heimdall", "Heimdall’s"),
        ] {
            let vocab = pieces.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let tokens = (0..vocab.len())
                .map(|id| Token {
                    id,
                    frame: id + 1,
                    duration: 1,
                    probability: 0.8,
                })
                .collect::<Vec<_>>();
            let config = crate::vocabulary::Vocabulary {
                words: vec![name.into()],
                ..Default::default()
            };
            let words = words_from_tokens_with_vocabulary(&tokens, &vocab, 0, 16000, Some(&config));
            assert_eq!(words.len(), 1, "{vocab:?}");
            assert_eq!(words[0].text, expected);
            assert_eq!(words[0].end_ns, vocab.len() as u64 * 80_000_000);
        }
    }
    #[test]
    fn phrase_entries_emit_one_word_per_phrase_word_only_when_completed() {
        let config = crate::vocabulary::Vocabulary {
            words: vec!["T3 Code".into()],
            ..Default::default()
        };
        let decode = |pieces: &[&str]| {
            let vocab: Vec<String> = pieces.iter().map(|p| (*p).to_owned()).collect();
            let tokens: Vec<_> = (0..vocab.len())
                .map(|id| Token { id, frame: id + 1, duration: 1, probability: 0.9 })
                .collect();
            words_from_tokens_with_vocabulary(&tokens, &vocab, 0, 32000, Some(&config))
        };
        // Exact spelling and the observed "Tifrico" route both end as two words.
        for pieces in [vec![" T", "3", " Co", "de", ","], vec![" T", "if", "ri", "co", "de", ","]] {
            let words = decode(&pieces);
            assert_eq!(words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["T3", "Code,"], "{pieces:?}");
            // The span's time is shared by character count, contiguous and ordered.
            assert_eq!(words[0].start_ns, 0);
            assert_eq!(words[0].end_ns, words[1].start_ns);
            assert!(words[0].end_ns < words[1].end_ns);
            assert_eq!(words[1].end_ns, pieces.len() as u64 * 80_000_000);
        }
        // Without the completing suffix nothing is rewritten.
        let words = decode(&[" T", "if", "ri", "co", "."]);
        assert_eq!(words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["Tifrico."]);
        let words = decode(&[" T", "-", "shirt"]);
        assert_eq!(words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["T-shirt"]);
    }
    #[test]
    fn joined_name_repair_checks_the_entire_tail_and_respects_real_boundaries() {
        for (tail, expected) in [
            (vec!["s", "ystem"], vec!["Heimdall", "system"]),
            (vec!["bug"], vec!["Heimdall", "bug"]),
            (vec![" s"], vec!["Heimdall", "s"]),
            (vec!["▁es"], vec!["Heimdall", "es"]),
            (vec!["e", "s", " are"], vec!["Heimdalles", "are"]),
        ] {
            let vocab = [vec![" heim", "dall"], tail]
                .concat()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let tokens = (0..vocab.len())
                .map(|id| Token {
                    id,
                    frame: id + 1,
                    duration: 1,
                    probability: 0.9,
                })
                .collect::<Vec<_>>();
            let words = words_from_tokens_with_vocabulary(
                &tokens,
                &vocab,
                0,
                16000,
                Some(&crate::vocabulary::Vocabulary::default()),
            );
            assert_eq!(
                words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
                expected
            );
        }
    }
    #[test]
    fn word_pieces_and_punctuation() {
        let vocab = vec!["▁look".into(), "▁at".into(), "▁this".into(), "!".into()];
        let tokens: Vec<_> = (0..4)
            .map(|id| Token {
                id,
                frame: id * 2 + 1,
                duration: 1,
                probability: 0.9,
            })
            .collect();
        let w = words_from_tokens(&tokens, &vocab, 1_000_000_000, 16000);
        assert_eq!(w[2].text, "this!");
        assert_eq!(w[0].start_ns, 1_000_000_000);
    }
    #[test]
    fn zero_duration_at_first_frame_uses_corrected_next_boundary() {
        let vocab = vec!["▁a".into(), "▁b".into()];
        let tokens = vec![
            Token {
                id: 0,
                frame: 0,
                duration: 0,
                probability: 1.0,
            },
            Token {
                id: 1,
                frame: 2,
                duration: 1,
                probability: 1.0,
            },
        ];
        let words = words_from_tokens(&tokens, &vocab, 1_000_000_000, 16000);
        assert_eq!(words[0].start_ns, 1_000_000_000);
        assert_eq!(words[0].end_ns, 1_080_000_000);
        assert_eq!(words[1].start_ns, words[0].end_ns);
    }
    #[test]
    fn seam_alignment_margin_does_not_duplicate_a_word() {
        let word = |text: &str, start_ns| Word {
            text: text.into(),
            start_ns,
            end_ns: start_ns + 320_000_000,
            confidence: 1.0,
        };
        let old = vec![
            word("while", 12_320_000_000),
            word("students", 12_560_000_000),
            word("often", 12_880_000_000),
        ];
        let new = vec![
            word("Students", 12_960_000_000),
            word("often", 13_120_000_000),
        ];
        let merged = reconcile(&old, &new, 12_960_000_000);
        assert_eq!(
            merged.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["while", "Students", "often"]
        );
    }
    #[test]
    fn overlap_preserves_repeated_words() {
        let word = |s: &str, t: u64| Word {
            text: s.into(),
            start_ns: t,
            end_ns: t + 80_000_000,
            confidence: 1.0,
        };
        let old = vec![word("go", 0), word("go", 2_000_000_000)];
        let new = vec![word("go", 2_080_000_000), word("now", 3_000_000_000)];
        let w = reconcile(&old, &new, 1_000_000_000);
        assert_eq!(
            w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["go", "go", "now"]
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod fault_model_tests {
    use super::*;
    use crate::gpu_recovery;
    #[test]
    #[ignore = "requires pinned offline ASR assets"]
    fn gpu_fault_asr_recreates_falls_back_restores_and_rejects_late_readback() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("../../models/parakeet-ultra");
        let audio = crate::file::read_wav(&root.join("tests/fixtures/en-2.wav"))?;
        let mut model = Parakeet::load_backend(&path, "metal")?;
        let expected = model.transcribe(&audio, 0)?;
        gpu_recovery::inject_missing();
        assert_eq!(model.transcribe(&audio, 0)?.words, expected.words);
        assert_eq!(model.backend(), "metal");
        // Encoder completes, then TDT loss: discarded tokens must not leak.
        gpu_recovery::inject_after(1, 1);
        assert_eq!(model.transcribe(&audio, 0)?.words, expected.words);
        assert_eq!(model.backend(), "metal");
        gpu_recovery::inject(2);
        let fallback = model.transcribe(&audio, 0)?;
        let mut cpu = Parakeet::load_backend(&path, "coreml")?;
        assert_eq!(fallback.words, cpu.transcribe(&audio, 0)?.words);
        assert_eq!(model.backend(), "coreml");
        assert!(gpu_recovery::gpu_status().is_some());
        assert_eq!(model.transcribe(&audio, 0)?.words, fallback.words);
        assert_eq!(model.backend(), "coreml");
        model.state.retry_now();
        assert_eq!(model.transcribe(&audio, 0)?.words, expected.words);
        assert_eq!(model.backend(), "metal");
        Ok(())
    }
    #[test]
    #[ignore = "requires pinned offline ASR assets"]
    fn gpu_fault_six_stream_batch_is_transactional() -> Result<()> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let audio = crate::file::read_wav(&root.join("tests/fixtures/en-2.wav"))?;
        let mut model = Parakeet::load_backend(&root.join("../../models/parakeet-ultra"), "metal")?;
        let inputs: Vec<_> = (0..6)
            .map(|n| (audio.as_slice(), n * 10_000_000_000))
            .collect();
        let expected = model
            .transcribe_batch(&inputs)
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        // Fail after one stream has decoded. No successful prefix escapes.
        gpu_recovery::inject_after(2, 1);
        let actual = model
            .transcribe_batch(&inputs)
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        for (a, b) in actual.iter().zip(&expected) {
            assert_eq!(a.words, b.words);
        }
        gpu_recovery::inject(2);
        assert_eq!(
            model
                .transcribe_batch(&inputs)
                .into_iter()
                .collect::<Result<Vec<_>>>()?
                .len(),
            6
        );
        assert_eq!(model.backend(), "coreml");
        model.state.retry_now();
        let actual = model
            .transcribe_batch(&inputs)
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        for (a, b) in actual.iter().zip(&expected) {
            assert_eq!(a.words, b.words);
        }
        Ok(())
    }
}
