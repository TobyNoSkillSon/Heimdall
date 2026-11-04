//! Phonon S: Gradium's 31M-parameter English TTS (`gradium/phonon-31m-en`, Apache-2.0), run in process on the CPU
//! through the `ptts` Rust runtime (`gradium-ai/xn-ptts`, MIT/Apache), with the 37 English voices of
//! `gradium/phonon-voices` as presets. Everything is read from the bundled model folder (`models/phonon-31m-en`):
//! `config.json`, `tokenizer.json`, `model.q8.gguf`, `voices.json` (the presets: group, name, catalog id,
//! description) and `voices/<id>.safetensors`. Nothing is downloaded: the crate is built without its `hub` feature.
//!
//! Measured on this M5 Max (lab/phonon/results.jsonl, Python wheel of the same runtime, 4 threads, 21 Oct): 44-54×
//! real time, 5 ms to first audio warm, ~300 MB resident.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

/// The bundled model folder's name under `models/`.
pub const MODEL_DIR: &str = "phonon-31m-en";
/// The preset list inside it.
pub const PRESETS_FILE: &str = "voices.json";
/// The preset a fresh choice of Phonon S starts with.
/// The public build's voice (firm, calm, British-leaning).
pub const DEFAULT_PRESET: &str = "Edwin";

/// One voice preset (`voices.json`): its menu group ("US women", "UK men", …), name, catalog id (the embedding's file
/// stem) and Gradium's description (the menu's tooltip).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct Preset {
    pub group: String,
    pub name: String,
    pub id: String,
    pub desc: String,
}

/// The presets in file order (grouped as written).
pub fn presets(model_dir: &Path) -> Result<Vec<Preset>> {
    let path = model_dir.join(PRESETS_FILE);
    let presets: Vec<Preset> = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))?;
    validate(&presets)?;
    Ok(presets)
}

/// Names unique and non-empty, ids safe as file stems.
pub fn validate(presets: &[Preset]) -> Result<()> {
    ensure!(!presets.is_empty(), "no Phonon presets");
    let mut names = std::collections::BTreeSet::new();
    for p in presets {
        ensure!(!p.name.trim().is_empty() && !p.group.trim().is_empty(), "a Phonon preset without a name or group");
        ensure!(!p.id.is_empty() && p.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "bad Phonon voice id {:?}", p.id);
        ensure!(names.insert(p.name.as_str()), "two Phonon presets named {}", p.name);
    }
    Ok(())
}

/// The groups in first-appearance order, each with its presets in file order (the submenu's sections).
pub fn grouped(presets: &[Preset]) -> Vec<(&str, Vec<&Preset>)> {
    let mut groups: Vec<(&str, Vec<&Preset>)> = vec![];
    for p in presets {
        match groups.iter_mut().find(|(g, _)| *g == p.group) {
            Some((_, list)) => list.push(p),
            None => groups.push((&p.group, vec![p])),
        }
    }
    groups
}

/// The embedding file of a preset.
pub fn voice_file(model_dir: &Path, preset: &Preset) -> PathBuf {
    model_dir.join("voices").join(format!("{}.safetensors", preset.id))
}

/// The loaded model with every preset registered under its name; one sentence at a time, in `voice`.
pub struct Phonon {
    synth: ptts::synth::Synth,
    sample_rate: u32,
    voice: String,
    names: Vec<String>,
}
impl Phonon {
    /// Loads the q8 model on the CPU and registers every preset (load ~1.2-1.6 s measured in the lab).
    pub fn load(model_dir: &Path) -> Result<Self> {
        let presets = presets(model_dir)?;
        let checkpoint = ptts::checkpoint::Checkpoint::resolve(model_dir, ptts::checkpoint::ResolveOptions { quant: ptts::synth::Quant::Q80, weights: None })
            .with_context(|| format!("Phonon S checkpoint at {}", model_dir.display()))?;
        let mut synth = checkpoint.builder(ptts::preprocess::Lang::En).device(ptts::synth::DeviceKind::Cpu).build().context("load Phonon S")?;
        let mut names = vec![];
        for preset in &presets {
            synth.add_voice_file(&preset.name, &voice_file(model_dir, preset)).with_context(|| format!("Phonon voice {}", preset.name))?;
            names.push(preset.name.clone());
        }
        let sample_rate = synth.sample_rate();
        Ok(Self { synth, sample_rate, voice: DEFAULT_PRESET.into(), names })
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    /// Choose the preset that speaks next; an unknown name keeps the current one and says so.
    pub fn set_voice(&mut self, name: &str) -> Result<()> {
        ensure!(self.names.iter().any(|n| n == name), "no Phonon preset named {name}");
        self.voice = name.into();
        Ok(())
    }
    pub fn voice(&self) -> &str {
        &self.voice
    }
}
impl crate::voice::StreamingVoice for Phonon {
    fn label(&self) -> &str {
        "Phonon S"
    }
    fn stream_sentence(&mut self, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
        let options = ptts::synth::SpeechOptions::default().voice(self.voice.clone());
        // Dropping the stream stops its generation threads (ptts joins them), so a cancel ends generation too.
        for chunk in self.synth.stream_with(text, &options).context("Phonon S")? {
            ensure!(!cancel(), "speech interrupted");
            let chunk = chunk.context("Phonon S")?;
            emit(&chunk, self.sample_rate)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab_presets() -> Option<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lab/phonon");
        dir.join(PRESETS_FILE).is_file().then_some(dir)
    }

    #[test]
    fn presets_parse_group_and_reject_duplicates() {
        let json = r#"[{"group":"US women","name":"Zoey","id":"NbpkqMVS3CJeq2j8","desc":"Playful."},
            {"group":"US men","name":"Easton","id":"LMDIYE0Oa377Qh6Z","desc":"Punchy."},
            {"group":"US women","name":"Hadley","id":"bXwrkHhURL1VFJdG","desc":"Kind."}]"#;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PRESETS_FILE), json).unwrap();
        let list = presets(dir.path()).unwrap();
        let groups = grouped(&list);
        assert_eq!(groups.iter().map(|(g, l)| (*g, l.len())).collect::<Vec<_>>(), [("US women", 2), ("US men", 1)]);
        assert_eq!(voice_file(dir.path(), &list[0]), dir.path().join("voices/NbpkqMVS3CJeq2j8.safetensors"));
        let mut dup = list.clone();
        dup[1].name = "Zoey".into();
        assert!(validate(&dup).is_err());
        let mut bad = list.clone();
        bad[0].id = "../x".into();
        assert!(validate(&bad).is_err());
    }

    /// The lab's list (the one bundled): 37 English presets, the default among them, groups as the user's sheet has them.
    #[test]
    fn the_lab_preset_list_has_37_english_voices_in_groups() {
        let Some(dir) = lab_presets() else { eprintln!("lab/phonon not present; skipped"); return };
        let list = presets(&dir).unwrap();
        assert_eq!(list.len(), 37);
        assert!(list.iter().any(|p| p.name == DEFAULT_PRESET));
        let groups: Vec<&str> = grouped(&list).iter().map(|(g, _)| *g).collect();
        assert_eq!(groups[..2], ["US women", "US men"]);
        assert!(groups.iter().any(|g| g.starts_with("UK")), "{groups:?}");
    }

    /// Opt-in model test: one short sentence through the real model on the CPU (needs the bundled or lab model).
    /// `cargo nextest run -p speak phonon_synthesizes --run-ignored only`
    #[test]
    #[ignore = "model inference (Phonon S on the CPU); opt-in"]
    fn phonon_synthesizes_one_sentence() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models").join(MODEL_DIR);
        let dir = std::env::var_os("HEIMDALL_PHONON_DIR").map(PathBuf::from).unwrap_or(bundled);
        let started = std::time::Instant::now();
        let mut model = Phonon::load(&dir).unwrap();
        let load = started.elapsed();
        model.set_voice("Hadley").unwrap();
        let mut samples = vec![];
        let mut first = None;
        let t = std::time::Instant::now();
        crate::voice::StreamingVoice::stream_sentence(&mut model, "Your build finished in forty two seconds, with no errors.", &|| false, &mut |chunk, _| {
            first.get_or_insert(t.elapsed());
            samples.extend_from_slice(chunk);
            Ok(())
        }).unwrap();
        let synth = t.elapsed();
        let seconds = samples.len() as f64 / model.sample_rate() as f64;
        let rms = (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len().max(1) as f64).sqrt();
        eprintln!("phonon: load {:.2} s, first audio {:.0} ms, {:.2} s of audio at {} Hz in {:.2} s ({:.1}x real time), rms {:.3}",
            load.as_secs_f64(), first.unwrap().as_secs_f64() * 1000., seconds, model.sample_rate(), synth.as_secs_f64(), seconds / synth.as_secs_f64(), rms);
        assert!(samples.iter().all(|s| s.is_finite()));
        assert!((1.5..8.).contains(&seconds), "{seconds}");
        assert!(rms > 0.01, "{rms}");
    }
}
