//! Background fill: generate missing takes for the current voice, only when the Mac is on AC power, the user
//! has been away from the keyboard and quiet, and Heimdall is not speaking. Never on the live path.
use crate::{ClipMeta, Generator, Line, Origin, Store, Stored};
use std::collections::{BTreeMap, BTreeSet};

/// Takes per line and voice. Several takes keep repeated acknowledgements from sounding identical.
pub const TAKES: usize = 3;
/// Same thresholds as the night pass: five minutes without keyboard/mouse input, two minutes without speech.
pub const IDLE_SECONDS: f64 = 300.;
pub const QUIET_SECONDS: f64 = 120.;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub line: Line,
    /// 1-based take number being generated.
    pub take: usize,
}
/// Take-major order: every line gets its first take before any line gets a second, so the bank becomes
/// usable as early as possible.
/// Lines in `exhausted` already have every distinct take their model produces (a deterministic model
/// repeats itself) and are skipped.
pub fn next_missing(
    lines: &[Line],
    have: &BTreeMap<String, Vec<i64>>,
    takes: usize,
    exhausted: &BTreeSet<String>,
) -> Option<Missing> {
    (1..=takes).find_map(|take| {
        lines
            .iter()
            .find(|l| !exhausted.contains(&l.id) && have.get(&l.id).map_or(0, Vec::len) < take)
            .map(|l| Missing { line: l.clone(), take })
    })
}

/// What the fill job sees before each clip. Unknown power counts as battery.
#[derive(Clone, Copy, Debug)]
pub struct Conditions {
    /// Heimdall on and Speaking on.
    pub permit: bool,
    pub on_ac: Option<bool>,
    pub user_idle_seconds: f64,
    pub speech_quiet_seconds: f64,
    /// Agent speech queued, synthesizing or playing, or a bank line playing.
    pub heimdall_speaking: bool,
}
impl Conditions {
    /// Why the gate is closed, or None when the fill may run.
    pub fn closed_by(&self) -> Option<&'static str> {
        if !self.permit {
            Some("speaking_or_heimdall_off")
        } else if self.on_ac != Some(true) {
            Some(if self.on_ac.is_none() { "power_unknown" } else { "battery" })
        } else if !(self.user_idle_seconds.is_finite() && self.user_idle_seconds >= IDLE_SECONDS) {
            Some("user_active")
        } else if self.speech_quiet_seconds.is_nan() || self.speech_quiet_seconds < QUIET_SECONDS {
            Some("speech_recent")
        } else if self.heimdall_speaking {
            Some("heimdall_speaking")
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum Tick {
    Closed(&'static str),
    Filled { missing: Missing, stored: Stored, synthesis_ms: f64 },
    /// The model reproduced an existing take exactly; the line is marked exhausted for this voice and model.
    Duplicate { missing: Missing },
    /// Every line has its takes for this voice and generator.
    Complete,
    Failed(String),
}
/// One fill step: check the gate, then generate and store at most one clip. `cancel` is checked inside
/// synthesis so speech or Speaking off stops a clip mid-way; a cancelled clip is not stored. `exhausted`
/// holds (voice, generator, line) triples whose model returned an identical take; the caller keeps it.
#[allow(clippy::too_many_arguments)]
pub fn tick(
    conditions: Conditions,
    store: &Store,
    lines: &[Line],
    voice: &str,
    generator: &mut dyn Generator,
    takes: usize,
    exhausted: &mut BTreeSet<(String, String, String)>,
    cancel: &dyn Fn() -> bool,
) -> Tick {
    tick_languages(conditions, store, lines, &BTreeMap::new(), &BTreeMap::new(), voice, generator, takes, exhausted, cancel)
}
/// [`tick`] for lines in several languages (the ack set): `languages` maps a line id to its language, `en` when
/// absent; `directions` maps an ack variant to its delivery direction, plain when absent. The generator is told
/// each take's language and direction before it speaks, and the take is stored with them (its settings).
#[allow(clippy::too_many_arguments)]
pub fn tick_languages(
    conditions: Conditions,
    store: &Store,
    lines: &[Line],
    languages: &BTreeMap<String, String>,
    directions: &BTreeMap<String, String>,
    voice: &str,
    generator: &mut dyn Generator,
    takes: usize,
    exhausted: &mut BTreeSet<(String, String, String)>,
    cancel: &dyn Fn() -> bool,
) -> Tick {
    if let Some(reason) = conditions.closed_by() {
        return Tick::Closed(reason);
    }
    let id = generator.id();
    let have = match store.takes(voice, &id) {
        Ok(have) => have,
        Err(e) => return Tick::Failed(format!("{e:#}")),
    };
    let skip: BTreeSet<String> =
        exhausted.iter().filter(|(v, g, _)| v == voice && *g == id).map(|(_, _, l)| l.clone()).collect();
    let Some(missing) = next_missing(lines, &have, takes, &skip) else { return Tick::Complete };
    let language = languages.get(&missing.line.id).map_or("en", String::as_str);
    let result = (|| {
        generator.set_language(language)?;
        generator.set_direction(directions.get(&missing.line.id).map(String::as_str))?;
        let audio = generator.synthesize(&missing.line.text, voice, cancel)?;
        anyhow::ensure!(!cancel(), "fill interrupted");
        let meta = ClipMeta {
            origin: Origin::Bank,
            line_id: Some(missing.line.id.clone()),
            text: missing.line.text.clone(),
            language: language.into(),
            voice: voice.into(),
            generator: id.clone(),
            settings: generator.settings(),
        };
        Ok((store.insert(&meta, &audio)?, audio.synthesis_ms))
    })();
    match result {
        Ok((stored, _)) if !stored.inserted => {
            exhausted.insert((voice.into(), id, missing.line.id.clone()));
            Tick::Duplicate { missing }
        }
        Ok((stored, synthesis_ms)) => Tick::Filled { missing, stored, synthesis_ms },
        Err(e) => Tick::Failed(format!("{e:#}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use anyhow::Result;
    /// Deterministic per text/voice/take so every take is distinct audio.
    pub(crate) struct Stub {
        pub id: &'static str,
        pub calls: usize,
    }
    impl Generator for Stub {
        fn id(&self) -> String {
            self.id.into()
        }
        fn settings(&self) -> serde_json::Value {
            serde_json::json!({"stub":self.id})
        }
        fn synthesize(&mut self, text: &str, voice: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
            anyhow::ensure!(!cancel(), "speech interrupted");
            self.calls += 1;
            let f = (text.len() + voice.len() * 7 + self.calls * 13 + self.id.len()) as f32 * 1e-3;
            Ok(speak::Audio {
                samples: (0..800).map(|i| (i as f32 * f).sin() * 0.2).collect(),
                sample_rate: 16_000,
                first_audio_ms: 0.,
                synthesis_ms: 1.,
            })
        }
    }
    pub(crate) fn open() -> Conditions {
        Conditions { permit: true, on_ac: Some(true), user_idle_seconds: 600., speech_quiet_seconds: 600., heimdall_speaking: false }
    }
    fn lines() -> Vec<Line> {
        ["sure", "okay", "done"].map(|id| Line { id: id.into(), text: format!("{id}.") }).to_vec()
    }
    fn fill(store: &Store, voice: &str, g: &mut dyn Generator) -> Vec<(String, usize)> {
        let mut out = vec![];
        loop {
            match tick(open(), store, &lines(), voice, g, 2, &mut BTreeSet::new(), &|| false) {
                Tick::Filled { missing, .. } => out.push((missing.line.id, missing.take)),
                Tick::Complete => return out,
                other => panic!("{other:?}"),
            }
        }
    }
    #[test]
    fn gate_requires_ac_idle_quiet_and_silence() {
        assert_eq!(open().closed_by(), None);
        type Close = fn(&mut Conditions);
        let cases: [(Close, &str); 7] = [
            (|c| c.permit = false, "speaking_or_heimdall_off"),
            (|c| c.on_ac = Some(false), "battery"),
            (|c| c.on_ac = None, "power_unknown"),
            (|c| c.user_idle_seconds = 299.9, "user_active"),
            (|c| c.user_idle_seconds = f64::NAN, "user_active"),
            (|c| c.speech_quiet_seconds = 30., "speech_recent"),
            (|c| c.heimdall_speaking = true, "heimdall_speaking"),
        ];
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path()).unwrap();
        for (close, reason) in cases {
            let mut c = open();
            close(&mut c);
            assert_eq!(c.closed_by(), Some(reason));
            let mut g = Stub { id: "stub-a", calls: 0 };
            assert!(matches!(tick(c, &store, &lines(), "M1", &mut g, 2, &mut BTreeSet::new(), &|| false), Tick::Closed(r) if r == reason));
            assert_eq!(g.calls, 0, "{reason}: nothing synthesized while closed");
        }
    }
    #[test]
    fn fills_take_major_then_completes_and_backend_swap_refills_without_touching_old_clips() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path()).unwrap();
        let mut a = Stub { id: "stub-a", calls: 0 };
        let order = fill(&store, "M1", &mut a);
        assert_eq!(
            order,
            [("sure", 1), ("okay", 1), ("done", 1), ("sure", 2), ("okay", 2), ("done", 2)].map(|(l, n)| (l.to_string(), n))
        );
        // Voice change: the new voice is missing everything; the old voice keeps its takes.
        assert_eq!(fill(&store, "F1", &mut a).len(), 6);
        // A new model: same lines, its own takes; old model's clips stay.
        let mut b = Stub { id: "stub-b", calls: 0 };
        assert_eq!(fill(&store, "M1", &mut b).len(), 6);
        assert_eq!(store.takes("M1", "stub-a").unwrap()["sure"].len(), 2);
        assert_eq!(store.takes("M1", "stub-b").unwrap()["sure"].len(), 2);
        let any = store.takes("M1", "stub-b").unwrap()["done"][0];
        assert_eq!(store.describe(any).unwrap()["settings"]["stub"], "stub-b");
        // A cancelled clip is not stored.
        let fresh = tempfile::tempdir().unwrap();
        let store = Store::open(fresh.path()).unwrap();
        assert!(matches!(tick(open(), &store, &lines(), "M1", &mut a, 1, &mut BTreeSet::new(), &|| true), Tick::Failed(_)));
        assert!(store.takes("M1", "stub-a").unwrap().is_empty());
    }
    /// Speaks English and Polish and records the language it was told before each take.
    struct Bilingual {
        stub: Stub,
        language: String,
        direction: Option<String>,
        heard: Vec<(String, String)>,
    }
    impl Generator for Bilingual {
        fn id(&self) -> String {
            self.stub.id()
        }
        fn settings(&self) -> serde_json::Value {
            serde_json::json!({"language":self.language,"direction":self.direction})
        }
        fn set_direction(&mut self, direction: Option<&str>) -> Result<()> {
            self.direction = direction.map(str::to_owned);
            Ok(())
        }
        fn set_language(&mut self, language: &str) -> Result<()> {
            anyhow::ensure!(["en", "pl"].contains(&language));
            self.language = language.into();
            Ok(())
        }
        fn synthesize(&mut self, text: &str, voice: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
            let heard = match &self.direction {
                Some(d) => format!("{}:{d}", self.language),
                None => self.language.clone(),
            };
            self.heard.push((text.into(), heard));
            self.stub.synthesize(text, voice, cancel)
        }
    }
    #[test]
    fn ack_set_fills_every_ack_twice_in_its_own_language() {
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path()).unwrap();
        // The user's store already has the reply line "on_it": the ack en_on_it still gets its own takes.
        let mut reply = Stub { id: "stub-a", calls: 0 };
        let on_it = vec![Line { id: "on_it".into(), text: "On it.".into() }];
        assert!(matches!(tick(open(), &store, &on_it, "M1", &mut reply, 1, &mut BTreeSet::new(), &|| false), Tick::Filled { .. }));
        let (lines, languages, directions) = (crate::acks::lines(), crate::acks::languages(), crate::acks::directions());
        assert_eq!(lines.len(), crate::acks::catalogue().iter().filter(|a| !a.retired()).count());
        let mut g = Bilingual { stub: Stub { id: "stub-a", calls: 0 }, language: String::new(), direction: None, heard: vec![] };
        // A generator without delivery control refuses a variant before speaking.
        let variant: Vec<Line> = lines.iter().filter(|l| directions.contains_key(&l.id)).take(1).cloned().collect();
        let mut plain_only = Stub { id: "stub-b", calls: 0 };
        assert!(matches!(tick_languages(open(), &store, &variant, &languages, &directions, "M1", &mut plain_only, 1, &mut BTreeSet::new(), &|| false), Tick::Failed(e) if e.contains("direction")));
        assert_eq!(plain_only.calls, 0);
        let mut filled = vec![];
        loop {
            match tick_languages(open(), &store, &lines, &languages, &directions, "M1", &mut g, crate::acks::FILL_TAKES, &mut BTreeSet::new(), &|| false) {
                Tick::Filled { missing, stored, .. } => filled.push((missing, stored.clip_id)),
                Tick::Complete => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(filled.len(), lines.len() * 2);
        let takes = store.takes("M1", "stub-a").unwrap();
        for ack in crate::acks::catalogue() {
            if ack.retired() {
                assert!(!takes.contains_key(&ack.id), "retired {} is not generated", ack.id);
                continue;
            }
            assert_eq!(takes[&ack.id].len(), 2, "{}", ack.id);
            for clip in &takes[&ack.id] {
                let meta = store.describe(*clip).unwrap();
                assert_eq!((meta["language"].as_str(), meta["text"].as_str()), (Some(ack.lang.as_str()), Some(ack.text.as_str())));
                assert_eq!(meta["settings"]["language"], ack.lang, "the generator was told the language before speaking");
                assert_eq!(meta["settings"]["direction"].as_str(), ack.delivery.as_ref().map(|d| d.direction.as_str()));
            }
        }
        assert!(g.heard.contains(&("On it.".into(), "en".into())));
        assert!(g.heard.contains(&("Stopping now.".into(), "en:crisp and urgent".into())), "variants are said with their delivery");
        assert_eq!(takes["on_it"].len(), 1, "reply-bank takes untouched");
        // An English-only generator refuses Polish before speaking and stores nothing.
        let fresh = tempfile::tempdir().unwrap();
        let store = Store::open(fresh.path()).unwrap();
        let polish = vec![Line { id: "pl_x".into(), text: "Sprawdzam.".into() }];
        let pl_language = BTreeMap::from([("pl_x".to_string(), "pl".to_string())]);
        let mut english = Stub { id: "stub-a", calls: 0 };
        assert!(matches!(tick_languages(open(), &store, &polish, &pl_language, &directions, "M1", &mut english, 1, &mut BTreeSet::new(), &|| false), Tick::Failed(e) if e.contains("English")));
        assert_eq!(english.calls, 0);
        assert!(store.takes("M1", "stub-a").unwrap().is_empty());
    }
    /// A deterministic model returns the same audio for every take: one take per line, then complete.
    #[test]
    fn deterministic_model_stops_after_one_distinct_take_per_line() {
        struct Same;
        impl Generator for Same {
            fn id(&self) -> String {
                "same".into()
            }
            fn settings(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            fn synthesize(&mut self, text: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
                Ok(speak::Audio { samples: vec![text.len() as f32 * 1e-3; 64], sample_rate: 16_000, first_audio_ms: 0., synthesis_ms: 0. })
            }
        }
        let t = tempfile::tempdir().unwrap();
        let store = Store::open(t.path()).unwrap();
        let mut exhausted = BTreeSet::new();
        let (mut filled, mut duplicates) = (0, 0);
        for _ in 0..20 {
            match tick(open(), &store, &lines(), "M1", &mut Same, TAKES, &mut exhausted, &|| false) {
                Tick::Filled { .. } => filled += 1,
                Tick::Duplicate { .. } => duplicates += 1,
                Tick::Complete => break,
                other => panic!("{other:?}"),
            }
        }
        // "sure." and "done." have equal length, so they share audio but keep separate line rows.
        assert_eq!((filled, duplicates), (3, 3));
    }
}
