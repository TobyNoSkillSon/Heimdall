use crate::embedding::{Embedding, MODEL_ID};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_STORE_WRITE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Calibration {
    pub threshold: f32,
    pub model_id: String,
    pub dataset: String,
    pub genuine_trials: usize,
    pub impostor_trials: usize,
    pub false_accept_rate: f64,
    pub false_reject_rate: f64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Trial {
    pub score: f32,
    pub same_speaker: bool,
}
impl Calibration {
    /// Empirical equal-error operating point. Trials must come from held-out speech, scored
    /// against the same max-over-enrolments rule used by `identify`, including unknown speakers.
    pub fn fit(trials: &[Trial], dataset: impl Into<String>) -> Result<Self> {
        ensure!(
            trials
                .iter()
                .all(|t| t.score.is_finite() && (-1.0..=1.0).contains(&t.score)),
            "invalid calibration scores"
        );
        let genuine_trials = trials.iter().filter(|t| t.same_speaker).count();
        let impostor_trials = trials.len() - genuine_trials;
        ensure!(
            genuine_trials > 0 && impostor_trials > 0,
            "calibration needs genuine and impostor trials"
        );
        let mut scores: Vec<_> = trials.iter().map(|t| t.score).collect();
        scores.sort_by(f32::total_cmp);
        scores.dedup();
        let mut thresholds = vec![-1., 1.000001];
        thresholds.extend(scores.windows(2).map(|w| (w[0] + w[1]) / 2.));
        thresholds.extend(scores.iter().copied());
        let rates = |threshold: f32| {
            let far = trials
                .iter()
                .filter(|t| !t.same_speaker && t.score >= threshold)
                .count() as f64
                / impostor_trials as f64;
            let frr = trials
                .iter()
                .filter(|t| t.same_speaker && t.score < threshold)
                .count() as f64
                / genuine_trials as f64;
            (far, frr)
        };
        let threshold = *thresholds
            .iter()
            .min_by(|a, b| {
                let (fa, fr) = rates(**a);
                let (ba, br) = rates(**b);
                (fa - fr)
                    .abs()
                    .total_cmp(&(ba - br).abs())
                    .then_with(|| (fa + fr).total_cmp(&(ba + br)))
                    .then_with(|| b.total_cmp(a))
            })
            .unwrap();
        let (false_accept_rate, false_reject_rate) = rates(threshold);
        let value = Self {
            threshold,
            model_id: MODEL_ID.into(),
            dataset: dataset.into(),
            genuine_trials,
            impostor_trials,
            false_accept_rate,
            false_reject_rate,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.model_id == MODEL_ID,
            "calibration belongs to a different checkpoint"
        );
        ensure!(
            self.threshold.is_finite() && (-1.0..=1.000001).contains(&self.threshold),
            "invalid calibrated threshold"
        );
        ensure!(
            !self.dataset.trim().is_empty() && self.genuine_trials > 0 && self.impostor_trials > 0,
            "missing calibration provenance"
        );
        ensure!(
            [self.false_accept_rate, self.false_reject_rate]
                .iter()
                .all(|x| x.is_finite() && (0.0..=1.0).contains(x)),
            "invalid calibration rates"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Enrollment {
    pub embeddings: Vec<Embedding>,
    /// Heimdall acts on this voice's requests during calls ("listen to Vim right now"): their
    /// lines that address Heimdall by name wake the agent. Off by default; on until the user turns it off (the Speakers
    /// window's switch or `listen_to`). Absent in older files and not written while off.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub listening: bool,
}
/// Names whose listening never changes: the user is always listened to (his microphone), Heimdall never (its own voice).
pub const NOT_GUESTS: [&str; 2] = ["User", "Heimdall"];
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnrollmentStore {
    pub version: u32,
    pub model_id: String,
    pub calibration: Option<Calibration>,
    pub names: BTreeMap<String, Enrollment>,
    /// Voices the user chose never to be asked about ("Don't ask again", bug note 2026-10-21T23-35-54), by id. A voice
    /// matching one is never prompted for; removing its entry (the Speakers window's Ask Again) undoes it. Absent in
    /// older files and not written while empty, so a store without ignored voices stays byte-identical.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ignored: BTreeMap<String, IgnoredVoice>,
    /// Quiet speaker naming: every voice no name matches gets a code label ("Speaker Fri 23 Oct
    /// 21h #3") and keeps its clean samples here, keyed by that label, so it is recognised again in later calls. No
    /// one is asked to name it; the voice namer (`crates/app/src/voice_namer.rs`) proposes a name only for a voice that
    /// comes back. Absent in older files and not written while empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub voices: BTreeMap<String, CodedVoice>,
    /// Heimdall's own voices ("generate a couple of lines in the background with no audio, run them
    /// through speaker recognition, and associate those voices with yourself"), keyed by the voice's sample key
    /// (`drama3`, `supertonic`, `phonon-Edwin`). Each is enrolled silently from lines it rendered, never played
    /// (`crates/app/src/pipeline/heimdall_voice.rs`). Matched under the name "Heimdall" by `identify`, and by the
    /// barge-in through speakers (`heimdall_similarity`). Absent in older files and not written while empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub heimdall: BTreeMap<String, HeimdallVoice>,
}
/// The name Heimdall's own voices are identified as.
pub const HEIMDALL: &str = "Heimdall";
/// A call track is Heimdall only this close to its voices, well above the people's calibrated threshold: a label of
/// Heimdall is never renamed, so a guest must never get it by a near match (its echo scores 0.5–0.9, people 0.3 or
/// less, 24 Oct).
pub const HEIMDALL_IDENTIFY_MIN: f32 = 0.6;
/// One of Heimdall's voices, enrolled from lines it rendered silently.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeimdallVoice {
    pub embeddings: Vec<Embedding>,
    /// How the lines were made into embeddings (the line set, speed and simulated speaker-to-microphone path). A
    /// different recipe enrols the voice again.
    pub recipe: String,
    /// Lines rendered.
    #[serde(default)]
    pub lines: usize,
    /// Unix milliseconds of the enrolment.
    #[serde(default)]
    pub enrolled_ms: u64,
}
/// Clean samples kept per coded voice: the first call's two windows and the newest ones after them.
pub const VOICE_SAMPLES: usize = 8;
/// A voice heard but not named: its code label is its key in `EnrollmentStore::voices`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodedVoice {
    pub embeddings: Vec<Embedding>,
    /// "Discord", "Call".
    #[serde(default)]
    pub heard_in: String,
    /// What it said when first coded.
    #[serde(default)]
    pub quote: String,
    /// Unix milliseconds: first and latest call it was recognised in.
    #[serde(default)]
    pub first_ms: u64,
    #[serde(default)]
    pub last_ms: u64,
    /// Calls it was recognised in (one count per call track).
    #[serde(default)]
    pub calls: u32,
    /// Local days it was heard on ("2026-10-23").
    #[serde(default)]
    pub days: BTreeSet<String>,
    /// The user chose Forget: never proposed for naming again. It keeps its code label and samples.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub irrelevant: bool,
}
/// A voice not to be asked about: its clean samples and where and when it was heard (for the Speakers window).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IgnoredVoice {
    pub embeddings: Vec<Embedding>,
    /// "Discord", "Call".
    #[serde(default)]
    pub heard_in: String,
    /// What it said when it was ignored.
    #[serde(default)]
    pub quote: String,
    /// Unix milliseconds when the user chose Don't ask again.
    #[serde(default)]
    pub at_ms: u64,
}
impl Default for EnrollmentStore {
    fn default() -> Self {
        Self {
            version: 1,
            model_id: MODEL_ID.into(),
            calibration: serde_json::from_str(include_str!("../tests/fixtures/calibration.json"))
                .ok(),
            names: BTreeMap::new(),
            ignored: BTreeMap::new(),
            voices: BTreeMap::new(),
            heimdall: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Identity {
    Known {
        name: String,
        cosine: f32,
    },
    Unknown {
        best_name: Option<String>,
        cosine: Option<f32>,
        reason: UnknownReason,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UnknownReason {
    NoEnrollment,
    Uncalibrated,
    BelowThreshold,
    Ambiguous,
}
impl EnrollmentStore {
    pub fn uncalibrated() -> Self {
        Self {
            calibration: None,
            ..Self::default()
        }
    }
    pub fn enroll(&mut self, name: &str, embedding: Embedding) -> Result<()> {
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.chars().count() <= 128,
            "invalid speaker name"
        );
        embedding.validate()?;
        self.names
            .entry(name.into())
            .or_insert_with(|| Enrollment {
                embeddings: Vec::new(),
                listening: false,
            })
            .embeddings
            .push(embedding);
        Ok(())
    }
    pub fn identify(&self, query: &Embedding) -> Identity {
        let heimdall = HEIMDALL.to_string();
        let mut scored: Vec<_> = self
            .names
            .iter()
            .filter_map(|(name, e)| {
                e.embeddings
                    .iter()
                    .map(|x| x.cosine(query))
                    .max_by(f32::total_cmp)
                    .map(|score| (name, score))
            })
            .chain(self.heimdall_similarity(query).filter(|(_, score)| *score >= HEIMDALL_IDENTIFY_MIN).map(|(_, score)| (&heimdall, score)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let Some((name, score)) = scored.first() else {
            return Identity::Unknown {
                best_name: None,
                cosine: None,
                reason: UnknownReason::NoEnrollment,
            };
        };
        let unknown = |reason| Identity::Unknown {
            best_name: Some((*name).clone()),
            cosine: Some(*score),
            reason,
        };
        let Some(cal) = &self.calibration else {
            return unknown(UnknownReason::Uncalibrated);
        };
        if cal.validate().is_err() || query.validate().is_err() {
            return unknown(UnknownReason::Uncalibrated);
        }
        if *score < cal.threshold {
            return unknown(UnknownReason::BelowThreshold);
        }
        if scored.get(1).is_some_and(|x| x.1 == *score) {
            return unknown(UnknownReason::Ambiguous);
        }
        Identity::Known {
            name: (*name).clone(),
            cosine: *score,
        }
    }
    /// The closest of Heimdall's own voices to `query` and its cosine (best over each voice's samples), with no
    /// threshold: the barge-in through speakers applies its own (`crates/app/src/pipeline/heimdall_voice.rs`).
    pub fn heimdall_similarity(&self, query: &Embedding) -> Option<(String, f32)> {
        self.heimdall
            .iter()
            .filter_map(|(key, v)| v.embeddings.iter().map(|x| x.cosine(query)).max_by(f32::total_cmp).map(|c| (key.clone(), c)))
            .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
    }
    /// Keeps `key`'s newly enrolled samples, replacing an older enrolment of that voice.
    pub fn set_heimdall_voice(&mut self, key: &str, voice: HeimdallVoice) -> Result<()> {
        ensure!(!key.trim().is_empty() && key.len() <= 128, "invalid voice key");
        ensure!(!voice.embeddings.is_empty(), "no voice samples to keep");
        for e in &voice.embeddings {
            e.validate()?;
        }
        self.heimdall.insert(key.into(), voice);
        Ok(())
    }
    /// The ignored voice `query` is, by the same rule `identify` uses for names (best score above the calibrated
    /// threshold, not tied).
    pub fn ignored_match(&self, query: &Embedding) -> Option<String> {
        if self.ignored.is_empty() {
            return None;
        }
        let as_names = EnrollmentStore {
            version: self.version,
            model_id: self.model_id.clone(),
            calibration: self.calibration.clone(),
            names: self.ignored.iter().map(|(id, v)| (id.clone(), Enrollment { embeddings: v.embeddings.clone(), listening: false })).collect(),
            ignored: BTreeMap::new(),
            voices: BTreeMap::new(),
            heimdall: BTreeMap::new(),
        };
        match as_names.identify(query) {
            Identity::Known { name, .. } => Some(name),
            Identity::Unknown { .. } => None,
        }
    }
    /// The coded voice `query` is (its code label), by the same rule as `identify`.
    pub fn voice_match(&self, query: &Embedding) -> Option<String> {
        if self.voices.is_empty() {
            return None;
        }
        let as_names = EnrollmentStore {
            version: self.version,
            model_id: self.model_id.clone(),
            calibration: self.calibration.clone(),
            names: self.voices.iter().map(|(code, v)| (code.clone(), Enrollment { embeddings: v.embeddings.clone(), listening: false })).collect(),
            ignored: BTreeMap::new(),
            voices: BTreeMap::new(),
            heimdall: BTreeMap::new(),
        };
        match as_names.identify(query) {
            Identity::Known { name, .. } => Some(name),
            Identity::Unknown { .. } => None,
        }
    }
    /// A new coded voice heard at `stamp` ("Fri 23 Oct 21h"): "Speaker Fri 23 Oct 21h #N", N the first free number
    /// in that hour. Returns the code.
    pub fn code_voice(&mut self, embeddings: Vec<Embedding>, heard_in: &str, quote: &str, at_ms: u64, stamp: &str, day: &str) -> Result<String> {
        ensure!(!embeddings.is_empty(), "no voice samples to keep");
        for e in &embeddings {
            e.validate()?;
        }
        let mut n = 1;
        let code = loop {
            let code = format!("Speaker {stamp} #{n}");
            if !self.voices.contains_key(&code) && !self.names.contains_key(&code) {
                break code;
            }
            n += 1;
        };
        let mut embeddings = embeddings;
        embeddings.truncate(VOICE_SAMPLES);
        self.voices.insert(code.clone(), CodedVoice {
            embeddings,
            heard_in: heard_in.trim().into(),
            quote: quote.trim().into(),
            first_ms: at_ms,
            last_ms: at_ms,
            calls: 1,
            days: BTreeSet::from([day.to_string()]),
            irrelevant: false,
        });
        Ok(code)
    }
    /// A coded voice recognised in another call: counted, and its samples kept (the first two, then the newest).
    pub fn heard_voice(&mut self, code: &str, embeddings: Vec<Embedding>, at_ms: u64, day: &str) -> Result<()> {
        let v = self.voices.get_mut(code).context("unknown voice")?;
        for e in embeddings {
            e.validate()?;
            if v.embeddings.len() >= VOICE_SAMPLES {
                v.embeddings.remove(2.min(v.embeddings.len() - 1));
            }
            v.embeddings.push(e);
        }
        v.last_ms = v.last_ms.max(at_ms);
        v.calls = v.calls.saturating_add(1);
        v.days.insert(day.into());
        Ok(())
    }
    /// Names a coded voice: its samples become `name`'s (joining an existing name's), and the code is retired.
    pub fn name_voice(&mut self, code: &str, name: &str) -> Result<()> {
        let v = self.voices.get(code).context("unknown voice")?.clone();
        let mut next = self.clone();
        for e in v.embeddings {
            next.enroll(name, e)?;
        }
        next.voices.remove(code);
        *self = next;
        Ok(())
    }
    /// Turns listening on or off for named voice `name` (see `Enrollment::listening`). The user and Heimdall are refused
    /// (`NOT_GUESTS`), as are coded voices: a guest is named before Heimdall listens to them.
    pub fn set_listening(&mut self, name: &str, on: bool) -> Result<()> {
        ensure!(!NOT_GUESTS.iter().any(|n| n.eq_ignore_ascii_case(name.trim())), "{name} is not a guest");
        self.names.get_mut(name).context("no named voice is called that")?.listening = on;
        Ok(())
    }
    /// Named voices Heimdall listens to.
    pub fn listened(&self) -> BTreeSet<String> {
        self.names.iter().filter(|(n, e)| e.listening && !NOT_GUESTS.iter().any(|g| g.eq_ignore_ascii_case(n))).map(|(n, _)| n.clone()).collect()
    }
    /// Forget: the voice is irrelevant and never proposed again (`false` undoes it). False when unknown.
    pub fn set_irrelevant(&mut self, code: &str, irrelevant: bool) -> bool {
        self.voices.get_mut(code).map(|v| v.irrelevant = irrelevant).is_some()
    }
    /// Remembers a voice as not to be asked about; returns its id.
    pub fn ignore(&mut self, embeddings: Vec<Embedding>, heard_in: &str, quote: &str, at_ms: u64) -> Result<String> {
        ensure!(!embeddings.is_empty(), "no voice samples to ignore");
        for e in &embeddings {
            e.validate()?;
        }
        let mut n = self.ignored.len() + 1;
        let id = loop {
            let id = format!("ignored-{at_ms}-{n}");
            if !self.ignored.contains_key(&id) {
                break id;
            }
            n += 1;
        };
        self.ignored.insert(id.clone(), IgnoredVoice { embeddings, heard_in: heard_in.trim().into(), quote: quote.trim().into(), at_ms });
        Ok(id)
    }
    /// Ask about this voice again: true when it was ignored.
    pub fn unignore(&mut self, id: &str) -> bool {
        self.ignored.remove(id).is_some()
    }
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(
            value.version == 1 && value.model_id == MODEL_ID,
            "incompatible enrollment store"
        );
        if let Some(c) = &value.calibration {
            c.validate()?;
        }
        for (name, e) in &value.names {
            ensure!(
                !name.trim().is_empty() && !e.embeddings.is_empty(),
                "invalid enrollment"
            );
            for x in &e.embeddings {
                x.validate()?;
            }
        }
        for (id, v) in &value.ignored {
            ensure!(!id.trim().is_empty() && !v.embeddings.is_empty(), "invalid ignored voice");
            for x in &v.embeddings {
                x.validate()?;
            }
        }
        for (code, v) in &value.voices {
            ensure!(!code.trim().is_empty() && !v.embeddings.is_empty(), "invalid coded voice");
            for x in &v.embeddings {
                x.validate()?;
            }
        }
        for (key, v) in &value.heimdall {
            ensure!(!key.trim().is_empty() && !v.embeddings.is_empty(), "invalid Heimdall voice");
            for x in &v.embeddings {
                x.validate()?;
            }
        }
        Ok(value)
    }
    /// Atomic, private replacement. Never persist a guessed name.
    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let nonce = NEXT_STORE_WRITE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".speakers-{}-{stamp}-{nonce}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut created = false;
        let result = (|| -> Result<()> {
            let mut file = options.open(&temp)?;
            created = true;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
            fs::rename(&temp, path).context("replace enrollment store")?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::DIMENSION;
    fn emb(i: usize) -> Embedding {
        let mut x = vec![0.; DIMENSION];
        x[i] = 1.;
        Embedding::new(x).unwrap()
    }
    fn calibration() -> Calibration {
        Calibration::fit(
            &[
                Trial {
                    score: 0.9,
                    same_speaker: true,
                },
                Trial {
                    score: 0.1,
                    same_speaker: false,
                },
            ],
            "test fixture",
        )
        .unwrap()
    }
    #[test]
    fn calibration_required_and_unknown_supported() {
        let mut s = EnrollmentStore::uncalibrated();
        s.enroll("Dave", emb(0)).unwrap();
        assert!(matches!(
            s.identify(&emb(0)),
            Identity::Unknown {
                reason: UnknownReason::Uncalibrated,
                ..
            }
        ));
        s.calibration = Some(calibration());
        assert!(matches!(s.identify(&emb(0)), Identity::Known { .. }));
        assert!(matches!(
            s.identify(&emb(1)),
            Identity::Unknown {
                reason: UnknownReason::BelowThreshold,
                ..
            }
        ));
        s.enroll("Anna", emb(0)).unwrap();
        assert!(matches!(
            s.identify(&emb(0)),
            Identity::Unknown {
                reason: UnknownReason::Ambiguous,
                ..
            }
        ));
    }
    #[test]
    fn no_invented_cutoff_and_corrupt_calibration_rejected() {
        assert!(Calibration::fit(&[], "empty").is_err());
        let mut c = calibration();
        c.threshold = f32::NAN;
        assert!(c.validate().is_err());
        c = calibration();
        c.model_id = "wrong model".into();
        assert!(c.validate().is_err());
    }
    /// Heimdall's own voices are named "Heimdall" only well above the people's threshold.
    #[test]
    fn heimdall_is_identified_only_close_to_its_own_voices() {
        let at = |c: f32| { let mut v = vec![0.; 192]; v[0] = c; v[1] = (1. - c * c).sqrt(); Embedding::new(v).unwrap() };
        let mut store = EnrollmentStore::default();
        store.set_heimdall_voice("drama3", HeimdallVoice { embeddings: vec![at(1.)], recipe: "r".into(), lines: 1, enrolled_ms: 0 }).unwrap();
        assert!(matches!(store.identify(&at(0.7)), Identity::Known { ref name, .. } if name == HEIMDALL));
        assert!(matches!(store.identify(&at(0.5)), Identity::Unknown { reason: UnknownReason::NoEnrollment, .. }), "a near match is nobody");
        assert_eq!(store.heimdall_similarity(&at(0.5)).map(|(k, _)| k), Some("drama3".into()));
        assert!(store.set_heimdall_voice("x", HeimdallVoice { embeddings: vec![], recipe: "r".into(), lines: 0, enrolled_ms: 0 }).is_err());
    }
    /// Listening is per named voice, off by default, saved, and never for the user or Heimdall.
    #[test]
    fn listening_is_per_named_voice_saved_and_never_for_owner_or_heimdall() {
        let mut store = EnrollmentStore::default();
        for name in ["Vim", "Dave", "Heimdall", "User"] {
            store.enroll(name, emb(0)).unwrap();
        }
        assert!(store.listened().is_empty(), "off by default");
        store.set_listening("Vim", true).unwrap();
        assert!(store.set_listening("Heimdall", true).is_err() && store.set_listening("owner", false).is_err());
        assert!(store.set_listening("Nobody", true).is_err());
        // A hand-edited file cannot make Heimdall a guest either.
        store.names.get_mut("Heimdall").unwrap().listening = true;
        assert_eq!(store.listened(), BTreeSet::from(["Vim".to_string()]));
        let path = std::env::temp_dir().join(format!("heimdall-test-listening-{}.json", std::process::id()));
        store.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"listening\"").count(), 2, "written only while on: {text}");
        let back = EnrollmentStore::load(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(back.names["Vim"].listening && !back.names["Dave"].listening);
        // Enrolling more samples keeps the flag.
        let mut back = back;
        back.enroll("Vim", emb(1)).unwrap();
        assert!(back.names["Vim"].listening);
    }
    #[test]
    fn private_store_roundtrip() {
        let p =
            std::env::temp_dir().join(format!("heimdall-test-store-{}.json", std::process::id()));
        let mut s = EnrollmentStore::default();
        s.enroll("Dave", emb(0)).unwrap();
        s.enroll("Dave", emb(1)).unwrap();
        s.save(&p).unwrap();
        let got = EnrollmentStore::load(&p).unwrap();
        assert_eq!(got.names["Dave"].embeddings.len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_file(p).unwrap();
    }
    #[test]
    fn shipped_cutoff_recomputes_from_measured_trials() {
        let trials: Vec<Trial> =
            serde_json::from_str(include_str!("../tests/fixtures/calibration-trials.json"))
                .unwrap();
        let bundled = EnrollmentStore::default().calibration.unwrap();
        let recomputed = Calibration::fit(&trials, &bundled.dataset).unwrap();
        assert_eq!(recomputed.threshold, bundled.threshold);
        assert_eq!(recomputed.false_accept_rate, bundled.false_accept_rate);
        assert_eq!(recomputed.false_reject_rate, bundled.false_reject_rate);
        bundled.validate().unwrap();
    }
}
