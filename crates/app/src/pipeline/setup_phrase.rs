//! Setup's last step, said: once Getting ready is all green, the user says "Heimdall, show me what
//! you can do". Heard on their microphone while Setup waits for it, it closes Setup (`UiUpdate::SetupHeard`), its words
//! are the agent's first message (`setup_wizard::greeting`), and its audio is the user's voice profile
//! (`UserVoice::enrol`), so the decider tells the user from other voices from then on.

use super::Pipeline;

/// The request the card asks for, after the wake name.
pub(crate) const REQUEST: &str = "show me what you can do";

/// Whether `text` is the phrase: the request, in any case and punctuation, with or without a wake name before it (the
/// ASR hears "Heimdall" as "Hindor" or "Hindol", and a custom name may come out any way). Close variants of the request
/// count ("could", "us", "what can you do").
pub(crate) fn heard(text: &str) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase().replace('’', "'"))
        .collect();
    let joined = format!(" {} ", words.join(" "));
    ["show me what you can do", "show me what you could do", "show us what you can do", "show me what can you do", "show me what u can do"]
        .iter()
        .any(|p| joined.contains(&format!(" {p} ")))
}

impl Pipeline {
    /// A final line of the user's own microphone: when it is the phrase and Setup waits for it, Setup finishes.
    pub(crate) fn setup_phrase_line(&mut self, text: &str, samples: &[f32]) {
        if !self.waits_for_phrase() || !heard(text) { return; }
        let said = text.trim().to_string();
        crate::diagnostics::line("setup", &format!("heard the last step's phrase ({} words): Setup closes", said.split_whitespace().count()));
        self.settings.first_run_said = Some(said);
        self.save_or_note();
        self.user_voice_enrol(samples);
        self.ui(ui::UiUpdate::SetupHeard);
    }

    /// Setup waits for the phrase: the public edition's first Setup, on Getting ready, with every row green (Start's own
    /// rule: the card shows then).
    pub(crate) fn waits_for_phrase(&self) -> bool {
        #[cfg(test)]
        if self.hooks.phrase_waits { return true; }
        if !crate::edition::PUBLIC || !crate::setup_wizard::setup_unfinished(&self.settings) { return false; }
        if self.settings.setup_step.as_deref() != Some(ui::SetupStep::Models.key().as_str()) { return false; }
        let models = crate::model_store::global().map(|d| crate::model_store::view(&d)).unwrap_or_default();
        models.finished() && self.setup_readiness().ready()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phrase as Parakeet writes it (VM runs 5 Nov: "Hindor", "Hindol"), with or without the name, in any case and
    /// punctuation; other talk never matches.
    #[test]
    fn the_phrase_matches_however_it_is_heard() {
        for said in [
            "Heimdall, show me what you can do.",
            "Hindor, show me what you can do",
            "hindol show me what you can do!",
            "Show me what you can do",
            "SHOW ME WHAT YOU CAN DO?",
            "Okay Heimdall... show me what you could do",
            "Selene, show us what you can do.",
            "Heimdall show me what can you do",
        ] {
            assert!(heard(said), "{said}");
        }
        for other in ["Did you catch the game last night?", "show me the build log", "what can you do", "Heimdall, what time is it?", "showme what you can do"] {
            assert!(!heard(other), "{other}");
        }
    }

    /// The phrase heard while Setup waits for it closes Setup, keeps the words for the agent's first
    /// message and makes the user's voice profile from its audio; other lines, or the phrase when Setup isn't waiting,
    /// do nothing.
    #[test]
    fn the_phrase_closes_setup_and_enrols_the_users_voice() {
        let (_t, mut a, _mic) = crate::pipeline::plumbing_tests::rig(false);
        let samples = vec![0.01f32; 16_000 * 2];
        a.setup_phrase_line("Hindor, show me what you can do", &samples);
        assert!(a.settings.first_run_said.is_none(), "Setup isn't waiting");
        a.hooks.phrase_waits = true;
        a.setup_phrase_line("Did you catch the game last night?", &samples);
        assert!(a.settings.first_run_said.is_none() && !a.heimdall_voice.user.ready());
        a.hooks.user_samples.push_back(speakers::embedding::Embedding::new(vec![1.; speakers::embedding::DIMENSION]).unwrap());
        a.setup_phrase_line("Hindor, show me what you can do.", &samples);
        assert_eq!(a.settings.first_run_said.as_deref(), Some("Hindor, show me what you can do."));
        assert!(a.heimdall_voice.user.ready(), "the phrase is the user's whole voice profile");
        assert!(a.hooks.ui.lock().unwrap().iter().any(|u| matches!(u, ui::UiUpdate::SetupHeard)), "Setup closes");
    }
}
