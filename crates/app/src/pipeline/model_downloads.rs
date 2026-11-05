//! The public edition's first-run downloads on the live loop (`crate::model_store`): Setup starts them when it leaves
//! the listening page, a launch after Setup resumes them, and listening waits for its models. The settings tick
//! notices when a group lands: listening starts by itself (live_loop.rs), and a decider precision that downloaded
//! after a fallback loaded is swapped in.
use super::*;
use crate::model_store;
use decide::precision::Precision;

/// Listening's models are in place: Parakeet, the speaker models, and a decider at some precision (the nearest one
/// loads while the chosen one downloads, as `precision::resolve` does for a missing folder).
pub(crate) fn listening_ready(ready: impl Fn(&str) -> bool) -> bool {
    ready(model_store::PARAKEET) && ready(model_store::SPEAKERS) && Precision::ALL.iter().any(|p| ready(&model_store::decider_group(*p)))
}

impl Pipeline {
    /// Setup left the listening page (or a launch after Setup): everything for the precision in settings.
    pub(crate) fn start_model_downloads(&mut self) {
        let Some(d) = model_store::global() else { return };
        // Setup's Back and a new precision: the earlier one, if still waiting, gives way.
        if !d.rows().is_empty() { d.choose_decider(&model_store::decider_group(self.settings.decider_precision)); }
        d.want(&model_store::first_run_groups(self.settings.decider_precision));
        self.publish_decider();
    }

    pub(crate) fn retry_model_downloads(&mut self) {
        if let Some(d) = model_store::global() { d.retry(); }
    }

    /// A precision chosen in the menu or Setup: downloaded when it is not here, once the first-run downloads have begun
    /// (before that, Setup's Continue starts them with the new precision).
    pub(crate) fn decider_precision_chosen(&mut self, p: Precision) {
        let Some(d) = model_store::global() else { return };
        if !d.rows().is_empty() {
            d.choose_decider(&model_store::decider_group(p));
        }
    }

    /// Whether listening may start: always in the personal edition (everything is bundled); in the public edition once
    /// its downloaded models are in place.
    pub(crate) fn listen_models_ready(&self) -> bool {
        if model_store::unavailable().is_some() { return false; }
        model_store::global().is_none_or(|d| listening_ready(|id| d.usable(id)))
    }

    /// The settings tick: when a group landed, the decider's picture is redrawn and a chosen precision that just
    /// arrived replaces the fallback running in its place. Returns whether listening's readiness changed.
    pub(crate) fn models_tick(&mut self) -> bool {
        if let Some(why) = model_store::unavailable() {
            self.component_status("models", Some(format!("Listening is off: Heimdall can\u{2019}t use its models folder ({why}).")));
            return false;
        }
        let Some(d) = model_store::global() else { return false };
        // Failed groups are tried again by themselves; the menu says why listening waits.
        d.tick();
        let line = listening_wait(&d.rows(), |id| d.usable(id), |id| d.retry_in(id));
        self.component_status("models", line);
        let seen = (listening_ready(|id| d.usable(id)), d.ready(&model_store::decider_group(self.settings.decider_precision)));
        if seen == self.models_seen { return false; }
        let listening_changed = seen.0 != self.models_seen.0;
        let decider_landed = seen.1 && !self.models_seen.1;
        self.models_seen = seen;
        if decider_landed { self.decider_download_landed(); } else { self.publish_decider(); }
        listening_changed
    }
}

/// Why listening is still off, for the menu (`None` once its models are in place, or before any download began): a
/// listening model that failed, with its reason and when it is tried again, else that they are downloading. No byte
/// counts: the line changes only when the reason does (Setup's Getting ready has the progress).
pub(crate) fn listening_wait(rows: &[model_store::Row], usable: impl Fn(&str) -> bool, retry_in: impl Fn(&str) -> Option<std::time::Duration>) -> Option<String> {
    if rows.is_empty() || listening_ready(&usable) { return None; }
    let listening = |id: &str| id == model_store::PARAKEET || id == model_store::SPEAKERS || id.starts_with("decider-");
    if let Some(r) = rows.iter().find(|r| listening(&r.id) && matches!(r.state, model_store::State::Failed(_))) {
        let model_store::State::Failed(why) = &r.state else { unreachable!() };
        let minutes = retry_in(&r.id).map_or(1, |d| d.as_secs().div_ceil(60).max(1));
        return Some(format!("Listening waits for {}: {}. Trying again in {minutes} min.", r.title, model_store::short(why)));
    }
    Some("Listening starts when its models finish downloading.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_waits_for_parakeet_the_speakers_and_any_decider() {
        let with = |ids: &'static [&'static str]| move |id: &str| ids.contains(&id);
        assert!(!listening_ready(with(&[])));
        assert!(!listening_ready(with(&["parakeet", "speakers"])), "no decider yet");
        assert!(!listening_ready(with(&["parakeet", "decider-4bit"])), "no speaker models yet");
        assert!(listening_ready(with(&["parakeet", "speakers", "decider-4bit"])));
        assert!(listening_ready(with(&["parakeet", "speakers", "decider-16bit"])), "8-bit chosen and downloading: 16-bit runs meanwhile");
        assert!(!listening_ready(with(&["speakers", "decider-4bit", "embedding"])), "memory search is not listening");
    }

    /// Review 5 Nov: after Setup, a failed listening model is named in the menu with when it is tried again.
    #[test]
    fn the_menu_says_why_listening_waits() {
        use model_store::{Row, State};
        let row = |id: &str, title: &str, state| Row { id: id.into(), title: title.into(), done: 0, total: 1, state };
        let none = |_: &str| None;
        assert_eq!(listening_wait(&[], |_| false, none), None, "nothing asked for yet");
        let downloading = [row("parakeet", "Parakeet", State::Downloading), row("decider-4bit", "Decider", State::Waiting)];
        assert_eq!(listening_wait(&downloading, |_| false, none).as_deref(), Some("Listening starts when its models finish downloading."));
        let failed = [row("parakeet", "Parakeet", State::Failed("parakeet-ultra/x: error sending request: dns error".into()))];
        assert_eq!(listening_wait(&failed, |_| false, |_| Some(std::time::Duration::from_secs(170))).as_deref(),
            Some("Listening waits for Parakeet: No connection to Hugging Face. Trying again in 3 min."));
        // Memory search failing does not hold listening.
        let ready = [row("parakeet", "Parakeet", State::Done), row("embedding", "EmbeddingGemma 2", State::Failed("x".into()))];
        assert_eq!(listening_wait(&ready, |id| id != "embedding", none), None);
        assert!(crate::pipeline::menu_problem::menu_problem("models", "Listening waits.", None).is_some(), "the menu shows it");
    }
}
