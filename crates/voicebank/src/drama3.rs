//! Offline bank generator: sequential resident Drama 3, with an ASR gate before
//! tick() can commit each take. Never substitutes Supertonic under Drama 3's id.
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct Drama3 {
    service: Arc<speak::drama3::Service>,
    assets: PathBuf,
    probe: PathBuf,
    scratch: tempfile::TempDir,
    asr: Value,
    /// The next take's language, given to the ASR check (Drama 3 itself takes text only, as in the app).
    language: String,
    /// The next take's delivery direction (an ack variant's), sent beside the plain words; none for plain takes.
    direction: Option<String>,
}
impl Drama3 {
    pub fn load(assets: PathBuf, probe: PathBuf, bank: &std::path::Path) -> Result<Self> {
        Self::load_with_splash_reserve(assets, probe, bank, "cap")
    }
    pub fn load_with_splash_reserve(assets: PathBuf, probe: PathBuf, bank: &std::path::Path, reserve: &str) -> Result<Self> {
        ensure!(probe.is_file(), "ASR probe is missing");
        let service = match reserve {
            "cap" => speak::drama3::Service::new(assets.clone()),
            "current" => speak::drama3::Service::new_offline_current(assets.clone(),
                heimdall_home::data_dir().join("settings.json")),
            _ => anyhow::bail!("--splash-reserve must be cap or current"),
        };
        service.configure(true);
        let deadline = Instant::now() + Duration::from_secs(48);
        while !service.ready() {
            let events = service.events();
            let failed = events.iter().any(|e| e["event"] == "failure");
            for event in events {
                eprintln!("{}", serde_json::json!({"drama3_event": event}));
            }
            if failed {
                anyhow::bail!("Drama 3 load failed: {:?}", service.status());
            }
            ensure!(Instant::now() < deadline, "Drama 3 load deadline");
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Self {
            service,
            assets,
            probe,
            scratch: tempfile::tempdir_in(bank)?,
            asr: Value::Null,
            language: "en".into(),
            direction: None,
        })
    }
}
impl crate::Generator for Drama3 {
    fn id(&self) -> String {
        speak::drama3::GENERATOR.into()
    }
    fn settings(&self) -> Value {
        let mut settings = speak::drama3::settings();
        settings["asr"] = self.asr.clone();
        if let Some(d) = &self.direction {
            settings["direction"] = d.as_str().into();
        }
        settings
    }
    /// Drama 3 has no language input: the app's own Jarvis path sends Polish agent speech as plain text too (the
    /// language reaches only the lighter fallback voice). The language goes to the ASR check, which must hear it.
    fn set_language(&mut self, language: &str) -> Result<()> {
        ensure!(LANGUAGES.contains(&language), "Drama 3 bank takes are en or pl, not {language}");
        self.language = language.into();
        Ok(())
    }
    fn set_direction(&mut self, direction: Option<&str>) -> Result<()> {
        if let Some(d) = direction {
            ensure!(speak::drama3::valid_direction(d), "invalid delivery direction {d:?}");
        }
        self.direction = direction.map(str::to_owned);
        Ok(())
    }
    fn synthesize(
        &mut self,
        text: &str,
        voice: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<speak::Audio> {
        ensure!(
            voice == speak::drama3::VOICE,
            "Drama 3 requires the Jarvis voice"
        );
        // Offline takes have their own whole-take ASR gate below and no live
        // listener to borrow. Render each sentence as a private first sentence;
        // never publish/play any of it until that existing whole-take gate passes.
        let (service, direction) = (&self.service, self.direction.as_deref());
        let rendered = render_take(text, &mut |sentence| match direction {
            Some(d) => service.render_directed(sentence, d, cancel),
            None => service.render(sentence, cancel),
        });
        // Keep the offline run's stall/memory evidence even when rendering fails.
        // The live app drains these events through diagnostics; this tool has no
        // app diagnostics owner and must not silently discard them.
        for event in self.service.events() {
            eprintln!("{}", serde_json::json!({"drama3_event": event}));
        }
        let audio = rendered?;
        let wav = self.scratch.path().join("take.wav");
        audio.write_wav(&wav)?;
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/drama3_asr.py");
        let mut child = Command::new(self.assets.join("runtime/bin/python3"))
            .arg("-B")
            .arg(script)
            .arg("--probe")
            .arg(&self.probe)
            .arg("--wav")
            .arg(&wav)
            .arg("--text")
            .arg(text)
            .arg("--language")
            .arg(&self.language)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let start = Instant::now();
        loop {
            if cancel() || start.elapsed() > Duration::from_secs(25) {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("ASR cancelled or timed out");
            }
            if child.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output()?;
        self.asr = accept_asr(output.status.success(), &output.stdout, &self.language)?;
        ensure!(!cancel(), "bank fill interrupted");
        Ok(audio)
    }
}
impl Drop for Drama3 {
    fn drop(&mut self) {
        self.service.stop();
    }
}

fn render_take(
    text: &str,
    render: &mut dyn FnMut(&str) -> Result<speak::Audio>,
) -> Result<speak::Audio> {
    let mut complete = None::<speak::Audio>;
    for sentence in speak::drama3::sentences(&speak::drama3::normalize(text)) {
        let mut next = render(&sentence)?;
        if let Some(audio) = complete.as_mut() {
            ensure!(
                audio.sample_rate == next.sample_rate,
                "bank sample rate changed"
            );
            audio.samples.append(&mut next.samples);
            audio.synthesis_ms += next.synthesis_ms;
        } else {
            complete = Some(next);
        }
    }
    complete.context("empty bank take")
}

/// Languages the bank's ASR gate checks (Parakeet v3 limited to English and Polish).
pub const LANGUAGES: &[&str] = &["en", "pl"];

/// The check must be an explicit pass made for the take's own language.
fn accept_asr(success: bool, bytes: &[u8], language: &str) -> Result<Value> {
    let asr: Value = serde_json::from_slice(bytes).context("ASR returned no check")?;
    ensure!(success && asr["passed"] == true, "ASR rejected take: {asr}");
    ensure!(asr["language"] == language, "ASR checked the take as {} instead of {language}: {asr}", asr["language"]);
    Ok(asr)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_multi_sentence_take_keeps_pcm_private_for_the_existing_whole_take_asr_gate() {
        let mut calls = Vec::new();
        let a = render_take("First. Later!", &mut |sentence| {
            calls.push(sentence.to_owned());
            Ok(speak::Audio {
                samples: vec![0.1; 64],
                sample_rate: 44100,
                first_audio_ms: 100.,
                synthesis_ms: 20.,
            })
        })
        .unwrap();
        assert_eq!(calls, ["First.", "Later!"]);
        assert_eq!(a.samples.len(), 128);
        assert_eq!(a.synthesis_ms, 40.);
        assert!(accept_asr(true, br#"{"passed":false}"#, "en").is_err());
    }
    #[test]
    fn bank_requires_a_successful_explicit_asr_pass() {
        assert!(accept_asr(true, br#"{"passed":true,"text":"Okay","language":"en"}"#, "en").is_ok());
        for (success, bytes) in [
            (false, br#"{"passed":true,"language":"en"}"#.as_slice()),
            (true, br#"{"passed":false,"language":"en"}"#.as_slice()),
            (true, b"{}".as_slice()),
            (true, b"".as_slice()),
        ] {
            assert!(accept_asr(success, bytes, "en").is_err());
        }
    }
    #[test]
    fn asr_pass_must_be_for_the_takes_language() {
        let pl = br#"{"passed":true,"text":"Sprawdzam.","expected":"Sprawdzam.","language":"pl"}"#;
        assert_eq!(accept_asr(true, pl, "pl").unwrap()["language"], "pl");
        assert!(accept_asr(true, pl, "en").is_err(), "a Polish check never passes an English take");
        // A check that does not say its language (an older script) is not a pass.
        assert!(accept_asr(true, br#"{"passed":true,"text":"Okay"}"#, "en").is_err());
    }
}
