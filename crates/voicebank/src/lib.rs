//! Voice bank: short English lines generated ahead of time so Heimdall can answer at the end of the user's turn
//! by playing a stored clip instead of synthesizing, plus the durable store of every voice clip Heimdall
//! generates (bank takes, agent speech) as training data for a later small speech model.
//!
//! Playback never synthesizes. Bank takes are generated only by the background fill job ([`fill`]); agent
//! replies are kept as they are spoken. Bank generation goes through one [`Generator`]
//! interface, so a new speech model is one new backend plus a refill.
pub mod acks;
pub mod ack_gate;
pub mod fill;
pub mod drama3;
pub mod lines;
pub mod import;
pub mod store;

pub use fill::{next_missing, tick, tick_languages, Conditions, Missing, Tick, TAKES};
pub use lines::{Line, LineSet, NONE};
pub use store::{ClipMeta, Origin, Store, Stored};

use anyhow::Result;
use serde_json::{json, Value};

/// The interface every speech model implements for the bank: text in a voice → audio.
pub trait Generator: Send {
    /// Stable identity of the model (weights and version). Bank clips are looked up by it, so a different
    /// model never plays another model's takes; swapping models means a new id and a refill.
    fn id(&self) -> String;
    /// Everything else that shapes the output (steps, speed, language, backend), stored with each clip.
    fn settings(&self) -> Value;
    fn synthesize(&mut self, text: &str, voice: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio>;
    /// The language (`en`, `pl`) of the following takes, set before each one. A generator that cannot speak and
    /// check a language refuses it; by default the bank is English only.
    fn set_language(&mut self, language: &str) -> Result<()> {
        anyhow::ensure!(language == "en", "{} fills English bank lines only, not {language}", self.id());
        Ok(())
    }
    /// The delivery direction of the following take (an ack variant's "crisp and urgent"), or none for plain
    /// speech; set before each take. A generator without delivery control refuses a direction.
    fn set_direction(&mut self, direction: Option<&str>) -> Result<()> {
        anyhow::ensure!(direction.is_none(), "{} cannot take a delivery direction", self.id());
        Ok(())
    }
}

/// The bundled placeholder model's identity in the bank.
pub const SUPERTONIC_ID: &str = "supertonic-3";
/// Supertonic's fixed render settings plus the backend that ran it (`auto`, `cpu`, `metal`).
pub fn supertonic_settings(backend: &str) -> Value {
    // f32 1.05 printed as f64 reads 1.0499999523; keep the setting as written.
    let speed = (speak::SPEED as f64 * 1000.).round() / 1000.;
    json!({"denoising_steps":speak::DENOISING_STEPS,"speed":speed,"language":"en","backend":backend})
}

/// Supertonic loaded directly (offline tools and the lab smoke). The app shares its already loaded model
/// through its own adapter instead of loading a second copy.
pub struct Supertonic {
    pub model: speak::Supertonic,
    pub backend: String,
}
impl Generator for Supertonic {
    fn id(&self) -> String {
        SUPERTONIC_ID.into()
    }
    fn settings(&self) -> Value {
        supertonic_settings(&self.backend)
    }
    fn synthesize(&mut self, text: &str, voice: &str, cancel: &dyn Fn() -> bool) -> Result<speak::Audio> {
        anyhow::ensure!(voice == speak::VOICE, "Supertonic speaks only {}", speak::VOICE);
        speak::Synthesizer::synthesize(&mut self.model, text, "en", cancel)
    }
}
