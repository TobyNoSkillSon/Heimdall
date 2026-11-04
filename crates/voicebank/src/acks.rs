//! Instant acknowledgements: when d1 sends the user's line to the agent, Heimdall says a short line in
//! Jarvis's voice ("On it.", "Will do.") after 1.5 s of quiet, before the agent has answered. The lines are compiled in
//! as `acks.json`, written by `lab/acks-1024/make_acks.py`. English only (the Polish takes "sound
//! so weird, especially with the accent"): a Polish turn finds no line in its language and gets no ack, never an
//! English one; a decider's Polish line is unknown and plays nothing either.
//!
//! 24 Oct set ("like Jarvis… a butler, but a butler with 'sure thing'"): plain base lines, which the
//! rule picks; delivery variants of them (`variant_of`, same words, a Drama 3 `delivery` direction such as "crisp and
//! urgent") for the scenarios they fit, chosen only by the decider; and retired lines (`replaced_by`), kept so a
//! decider trained on them still resolves to the line that replaced them. They are never generated or picked.
//!
//! Until decider v3 learns to choose them, the line is picked by rule: the turn's language, then a pool from its words
//! (a screen phrase, a note, a question, otherwise an action), rotating so the same line never plays twice running.
//! `specific` lines (stop, worker, changing course…) are only for the decider and never picked by the rule.
//!
//! Takes are bank clips stored under the ack's own id; an ack whose text is identical to a reply-bank line
//! (`bank_line`) also uses that line's takes, so the English acks recorded in Jarvis already play before the ack set
//! itself is generated.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pool {
    Action,
    Question,
    Look,
    Note,
    /// Only for the decider: never picked by the rule.
    Specific,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// Stable id (`en_on_it`): the clips' line id.
    pub id: String,
    /// `en` or `pl`.
    pub lang: String,
    pub text: String,
    pub pool: Pool,
    /// When it fits (the decider's description).
    #[serde(rename = "use")]
    pub use_: String,
    /// A reply-bank line with identical text whose takes this ack may play.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bank_line: Option<String>,
    /// The base ack this is a delivery variant of (same text and language). Variants are the decider's only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant_of: Option<String>,
    /// How the takes were said; none for the plain base lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// The situations this ack's delivery was written for (decider training examples).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenarios: Vec<Scenario>,
    /// Retired: the current ack that took over its use. A retired ack has no takes of its own to generate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaced_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// Short label (`urgent`, `dry`, `low`).
    pub tag: String,
    /// What Drama 3 is told before the words (`speak::drama3::valid_direction`), e.g. "crisp and urgent".
    pub direction: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable id (`late_night`), the same situation text wherever it appears.
    pub id: String,
    pub situation: String,
}

impl Ack {
    pub fn retired(&self) -> bool {
        self.replaced_by.is_some()
    }
    /// Whether the rule may pick it: a current plain line (no delivery variant, not retired).
    pub fn rule_pick(&self) -> bool {
        !self.retired() && self.variant_of.is_none()
    }
}

pub const ACKS_JSON: &str = include_str!("../acks.json");

/// The compiled ack set.
pub fn catalogue() -> &'static [Ack] {
    static ACKS: OnceLock<Vec<Ack>> = OnceLock::new();
    ACKS.get_or_init(|| serde_json::from_str(ACKS_JSON).expect("bundled acks.json is valid"))
}
pub fn by_id(id: &str) -> Option<&'static Ack> {
    catalogue().iter().find(|a| a.id == id)
}
/// The ack to play for `id`: itself, or for a retired ack the one that replaced it.
pub fn resolve(id: &str) -> Option<&'static Ack> {
    let ack = by_id(id)?;
    match &ack.replaced_by {
        Some(next) => by_id(next).filter(|a| !a.retired()),
        None => Some(ack),
    }
}

/// The decider's line `id` and its takes: the current ack (a retired one's replacement); until that has takes, a
/// retired line plays its own older takes, so a store filled before the replacement keeps acknowledging.
pub fn decider_takes(id: &str, bank: &BTreeMap<String, Vec<i64>>) -> Option<(&'static Ack, Vec<i64>)> {
    let current = resolve(id)?;
    let clips = takes(current, bank);
    if !clips.is_empty() {
        return Some((current, clips));
    }
    let own = by_id(id).filter(|a| a.retired())?;
    let clips = takes(own, bank);
    (!clips.is_empty()).then_some((own, clips))
}
/// The takes of `ack` among a store's bank takes (line id → clip ids): its own first, then its reply-bank line's.
pub fn takes(ack: &Ack, bank: &BTreeMap<String, Vec<i64>>) -> Vec<i64> {
    let own = bank.get(&ack.id).into_iter().flatten();
    let shared = ack.bank_line.as_ref().and_then(|l| bank.get(l)).into_iter().flatten();
    own.chain(shared).copied().collect()
}

/// The language to acknowledge a turn in. Polish needs real evidence: a Polish word (Parakeet often writes "Dobra,
/// odpal build" without any letters), or Polish letters in at least two words; and such words make up at least a
/// fifth of the turn. One accented word is not enough: ASR heard "Halló Morgan?" (23 Oct). English otherwise.
pub fn language(text: &str) -> &'static str {
    const LETTERS: &str = "ąćęłńóśźżĄĆĘŁŃÓŚŹŻ";
    const POLISH: &[&str] = &[
        // Looked up without Polish letters ("zrób" → "zrob"). Words that are also English ("ten", "na", "mi",
        // "ale", "tu") are left out.
        "sie", "ze", "nie", "jest", "czy", "co", "jak", "mozesz", "zrob", "prosze", "dobra", "odpal", "sprawdz",
        "zobacz", "mnie", "tego", "tak", "dlaczego", "gdzie", "kiedy", "ile", "jeszcze", "teraz", "juz",
        "tez", "moze", "bedzie", "bo", "wiec", "tylko", "jakis", "cos", "tam", "sa", "byl", "zeby",
        "dzieki", "otworz", "napisz", "zapisz", "pokaz", "daj", "wlacz", "wylacz", "zrobic", "dla", "przez",
        "czesc", "dzien", "dobry",
    ];
    let plain = |w: &str| -> String {
        w.chars()
            .map(|c| match c {
                'ą' => 'a', 'ć' => 'c', 'ę' => 'e', 'ł' => 'l', 'ń' => 'n', 'ó' => 'o', 'ś' => 's', 'ź' | 'ż' => 'z',
                c => c,
            })
            .collect()
    };
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let (mut known, mut lettered, mut evidence) = (0, 0, 0);
    for w in &words {
        let k = POLISH.contains(&plain(w).as_str());
        let l = w.chars().any(|c| LETTERS.contains(c));
        known += k as usize;
        lettered += l as usize;
        evidence += (k || l) as usize;
    }
    if (known > 0 || lettered >= 2) && evidence * 5 >= words.len() { "pl" } else { "en" }
}

/// Takes the offline fill generates per ack (`heimdall-voicebank fill --set acks`): two, for variety.
pub const FILL_TAKES: usize = 2;
/// Every current ack as a bank line (id and exact text), for the offline fill. Acks with a `bank_line` get their own
/// takes too; retired acks get none.
pub fn lines() -> Vec<crate::Line> {
    catalogue().iter().filter(|a| !a.retired()).map(|a| crate::Line { id: a.id.clone(), text: a.text.clone() }).collect()
}
/// Ack id → delivery direction, for the fill's variant takes.
pub fn directions() -> BTreeMap<String, String> {
    catalogue().iter().filter_map(|a| Some((a.id.clone(), a.delivery.as_ref()?.direction.clone()))).collect()
}
/// Ack id → language, for [`crate::fill::tick_languages`].
pub fn languages() -> BTreeMap<String, String> {
    catalogue().iter().map(|a| (a.id.clone(), a.lang.clone())).collect()
}

/// Which pool a turn's words call for.
pub fn pool_for(text: &str) -> Pool {
    let t = text.to_lowercase();
    let words: Vec<&str> = t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |phrase: &str| {
        let p: Vec<&str> = phrase.split(' ').collect();
        words.windows(p.len()).any(|w| w == p.as_slice())
    };
    const LOOK: &[&str] = &[
        "look at this", "take a look", "check this out", "screenshot", "what is this", "what's this", "see this",
        "zobacz", "spójrz", "popatrz", "zerknij", "zrzut ekranu", "co to jest",
    ];
    const NOTE: &[&str] = &[
        "note", "remember", "write down", "add to the list", "add it to the list", "bug note", "zanotuj", "zapamiętaj",
        "zapisz", "dopisz", "notatka",
    ];
    if LOOK.iter().any(|p| has(p)) {
        return Pool::Look;
    }
    if NOTE.iter().any(|p| has(p)) {
        return Pool::Note;
    }
    // A polite request ("can you…", "could you…", "możesz…") asks for an action, not an answer.
    const REQUEST: &[&str] = &["can", "could", "would", "will", "możesz", "mógłbyś", "zrobisz"];
    // Words that open only questions; "is", "do", "have"… open commands too ("Do the migration"), so those turns
    // count as questions only with their question mark.
    const ASK: &[&str] = &[
        "what", "why", "how", "when", "where", "who", "which", "whats",
        "co", "czy", "jak", "dlaczego", "czemu", "kiedy", "gdzie", "kto", "ile", "który", "która", "które",
    ];
    // Lead words before the question proper ("Heimdall, what…", "okay so why…").
    const LEAD: &[&str] = &["heimdall", "hey", "okay", "ok", "so", "and", "but", "hmm", "um", "no", "dobra", "a", "i", "to"];
    let first = words.iter().find(|w| !LEAD.contains(w)).copied().unwrap_or("");
    if REQUEST.contains(&first) {
        return Pool::Action;
    }
    if t.trim_end().ends_with('?') || ASK.contains(&first) {
        return Pool::Question;
    }
    Pool::Action
}

/// Picks the ack for a turn: its language, its pool (falling back to the action pool when that pool has no takes),
/// rotating so the last `avoid` acks are not repeated while another candidate exists. Returns the ack and a take.
pub fn choose(
    lang: &str,
    pool: Pool,
    bank: &BTreeMap<String, Vec<i64>>,
    recent: &[String],
    turn_seed: u64,
) -> Option<(&'static Ack, i64)> {
    let candidates = |pool: Pool| -> Vec<(&'static Ack, Vec<i64>)> {
        catalogue()
            .iter()
            .filter(|a| a.lang == lang && a.pool == pool && a.rule_pick())
            .map(|a| (a, takes(a, bank)))
            .filter(|(_, t)| !t.is_empty())
            .collect()
    };
    let mut found = candidates(pool);
    if found.is_empty() && pool != Pool::Action {
        found = candidates(Pool::Action);
    }
    if found.is_empty() {
        return None;
    }
    // Prefer acks not said recently; the least recently said when all were.
    let fresh: Vec<_> = found.iter().filter(|(a, _)| !recent.contains(&a.id)).collect();
    let (ack, takes) = if fresh.is_empty() {
        found
            .iter()
            .min_by_key(|(a, _)| recent.iter().rposition(|r| r == &a.id))
            .unwrap()
    } else {
        fresh[(turn_seed % fresh.len() as u64) as usize]
    };
    let take = takes[((turn_seed / 7) % takes.len() as u64) as usize];
    Some((ack, take))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_is_the_decider_set_with_valid_ids_and_both_languages() {
        let acks = catalogue();
        let ids: std::collections::BTreeSet<_> = acks.iter().map(|a| &a.id).collect();
        assert_eq!(ids.len(), acks.len(), "ids are unique");
        // After the user's ratings (23 Oct 22:25): two plain lines and two deliveries cut.
        assert_eq!(acks.iter().filter(|a| a.rule_pick()).count(), 30);
        assert_eq!(acks.iter().filter(|a| a.variant_of.is_some()).count(), 18);
        assert_eq!(acks.iter().filter(|a| a.retired()).count(), 10);
        assert_eq!(lines().len(), 48, "every current line and variant is generated");
        for cut in ["en_leave_it_with_me__steady", "en_got_it_changing_course__crisp"] {
            assert!(by_id(cut).is_none(), "{cut}");
        }
        assert_eq!(resolve("en_let_me_take_a_look").unwrap().id, "en_having_a_look");
        assert_eq!(resolve("en_pulling_it_up").unwrap().id, "en_on_it");
        assert!(acks.iter().all(|a| a.lang == "en"), "English only");
        for a in acks {
            assert!(a.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'), "{}", a.id);
            assert!(a.id.starts_with(&format!("{}_", a.lang)), "{}", a.id);
            assert!(!a.text.trim().is_empty() && a.text.len() <= 200);
        }
        for pool in [Pool::Action, Pool::Question, Pool::Look, Pool::Note] {
            assert!(acks.iter().any(|a| a.pool == pool), "{pool:?}");
        }
        // Old English ids stay resolvable (decider v3 was trained on last night's 54); the Polish ones are gone.
        for old in ["en_okay", "en_okay_stopping", "en_on_it"] {
            let a = resolve(old).unwrap_or_else(|| panic!("{old}"));
            assert!(!a.retired() && a.lang == old[..2], "{old}");
        }
        assert_eq!(resolve("en_okay").unwrap().id, "en_right_away");
        assert!(resolve("en_dance").is_none() && resolve("pl_sprawdzam").is_none() && resolve("pl_dobra").is_none());
        // The shared reply-bank lines exist with the same text.
        let seed = crate::lines::seed();
        for a in acks.iter().filter(|a| a.bank_line.is_some()) {
            let line = seed.iter().find(|l| Some(&l.id) == a.bank_line.as_ref()).expect("bank line exists");
            assert_eq!(line.text, a.text);
        }
    }

    /// "Okay" sounded weird, "Jasne" is "not really manly"; "sir" sparingly; each line short.
    #[test]
    fn current_lines_keep_the_butler_character_and_variants_match_their_base() {
        let current: Vec<_> = catalogue().iter().filter(|a| !a.retired()).collect();
        for a in &current {
            let first = a.text.split(|c: char| !c.is_alphanumeric()).next().unwrap().to_lowercase();
            assert!(!["okay", "ok", "okej", "jasne", "dobra"].contains(&first.as_str()), "{}", a.id);
            // About 1.2 s spoken (last night's takes: "I'll get a worker on it." 1.16-1.21 s, "Let me think about
            // that." 1.49-1.9 s); durations are measured per take in the lab player.
            assert!(a.text.chars().count() <= 24, "short enough for about 1.2 s: {}", a.id);
        }
        let sir: std::collections::BTreeSet<_> =
            current.iter().filter(|a| a.text.contains("sir")).map(|a| a.variant_of.as_deref().unwrap_or(&a.id)).collect();
        assert!(sir.len() <= 2, "sir sparingly: {sir:?}");
        let mut situations = BTreeMap::new();
        for a in catalogue() {
            for s in &a.scenarios {
                assert_eq!(situations.entry(s.id.clone()).or_insert(s.situation.clone()), &s.situation, "{}", s.id);
            }
            if let Some(base) = &a.variant_of {
                // Its base may be retired (the user cut the plain "Let me take a look." but kept its attentive take).
                let b = by_id(base).expect("variant's base exists");
                assert!(b.variant_of.is_none() && b.text == a.text && b.lang == a.lang && b.pool == a.pool, "{}", a.id);
                let d = a.delivery.as_ref().expect("a variant has a delivery");
                assert_eq!(a.id, format!("{base}__{}", d.tag));
                assert!(speak::drama3::valid_direction(&d.direction), "{}", a.id);
                assert!(!a.scenarios.is_empty() && !a.retired(), "{}", a.id);
            } else {
                assert!(a.delivery.is_none(), "base lines are plain: {}", a.id);
            }
            if let Some(next) = &a.replaced_by {
                assert!(resolve(&a.id).is_some_and(|n| &n.id == next), "{}", a.id);
                assert!(a.bank_line.is_none() && a.variant_of.is_none(), "{}", a.id);
            }
        }
        for id in ["calm", "rushed", "frustrated", "joking", "urgent_order", "quick_question", "look_at_screen", "late_night"] {
            assert!(situations.contains_key(id), "scenario {id}");
        }
        assert_eq!(directions().len(), 18);
    }

    #[test]
    fn last_nights_schema_still_loads() {
        let old: Vec<Ack> = serde_json::from_str(r#"[{"id":"en_okay","lang":"en","text":"Okay.","pool":"action","use":"Minimal ack.","bank_line":"okay"}]"#).unwrap();
        assert!(old[0].rule_pick() && old[0].delivery.is_none() && old[0].scenarios.is_empty());
        let round: Vec<Ack> = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(round, old);
        assert!(!serde_json::to_string(&old).unwrap().contains("scenarios"), "absent fields stay absent");
    }

    #[test]
    fn decider_v3_english_lines_resolve_and_polish_ones_play_nothing() {
        let question: serde_json::Value = serde_json::from_str(include_str!("../../decide/questions/dispatch-v3.json")).unwrap();
        for lang in ["en", "pl"] {
            for key in question["ack"][lang]["criteria"].as_object().unwrap().keys() {
                if ["send", "wait", "not_for_heimdall"].contains(&key.as_str()) {
                    continue;
                }
                let id = format!("{lang}_{key}");
                match lang {
                    "en" => assert_eq!(resolve(&id).unwrap_or_else(|| panic!("{id}")).lang, "en"),
                    _ => assert!(resolve(&id).is_none() && decider_takes(&id, &BTreeMap::from([(id.clone(), vec![1])])).is_none(), "{id}"),
                }
            }
        }
    }

    #[test]
    fn language_follows_letters_or_polish_words() {
        assert_eq!(language("Zrób zrzut ekranu"), "pl");
        assert_eq!(language("Dobra, odpal build"), "pl");
        assert_eq!(language("czy mozesz to sprawdzic"), "pl");
        assert_eq!(language("Can you run the build on the island?"), "en");
        assert_eq!(language("Open the island and take a look"), "en");
        assert_eq!(language("Heimdall"), "en");
    }

    /// 23 Oct: ASR wrote "Halló Morgan?" and the ack was "Sprawdzam.". One accented word is not Polish.
    #[test]
    fn one_accented_word_does_not_make_a_turn_polish() {
        assert_eq!(language("Halló Morgan?"), "en");
        assert_eq!(language("Halló"), "en");
        assert_eq!(language("Can you ask Łukasz about the build?"), "en");
        for pl in ["Sprawdź pogodę", "co tam słychać", "co tam slychac", "zrób to", "zrob to", "Cześć", "Dzięki!", "Włącz światło"] {
            assert_eq!(language(pl), "pl", "{pl}");
        }
    }

    #[test]
    fn pools_follow_the_words_in_both_languages() {
        assert_eq!(pool_for("Heimdall, look at this error"), Pool::Look);
        assert_eq!(pool_for("zobacz to, coś tu nie działa"), Pool::Look);
        assert_eq!(pool_for("Note that the build broke at noon."), Pool::Note);
        assert_eq!(pool_for("Zanotuj, że trzeba zadzwonić."), Pool::Note);
        assert_eq!(pool_for("What's the weather in Poznań?"), Pool::Question);
        assert_eq!(pool_for("okay so why is the build slow"), Pool::Question);
        assert_eq!(pool_for("Czy build już przeszedł?"), Pool::Question);
        assert_eq!(pool_for("Can you fix the island?"), Pool::Action, "a polite request is an action");
        assert_eq!(pool_for("Możesz odpalić testy?"), Pool::Action);
        assert_eq!(pool_for("Open Safari and start the timer."), Pool::Action);
        assert_eq!(pool_for("Odpal build."), Pool::Action);
        assert_eq!(pool_for("Do the migration."), Pool::Action, "an imperative 'do' is an action");
        assert_eq!(pool_for("Is the build green?"), Pool::Question);
        assert_eq!(language("Remind me in ten minutes"), "en");
        assert_eq!(language("Na pewno?"), "en", "too short to tell without Polish letters or words");
    }

    #[test]
    fn choice_keeps_the_language_varies_and_falls_back_to_shared_english_takes() {
        // Only the reply bank's English takes (as on the user's Mac today): English acks play, Polish ones don't.
        let mut bank = BTreeMap::new();
        for (line, clip) in [("on_it", 1), ("will_do", 2), ("okay", 3), ("got_it", 4), ("let_me_check", 5)] {
            bank.insert(line.to_string(), vec![clip]);
        }
        assert!(choose("pl", Pool::Action, &bank, &[], 0).is_none(), "never answers Polish in English");
        let (ack, _) = choose("en", Pool::Question, &bank, &[], 0).unwrap();
        assert_eq!(ack.id, "en_let_me_check");
        let (ack, _) = choose("en", Pool::Look, &bank, &[], 0).unwrap();
        assert_eq!(ack.pool, Pool::Action, "no look take yet: an action ack");
        // Rotation: four turns in a row never repeat the previous ack.
        let mut recent: Vec<String> = vec![];
        for seed in 0..12 {
            let (ack, take) = choose("en", Pool::Action, &bank, &recent, seed).unwrap();
            assert!(recent.last() != Some(&ack.id), "repeated {}", ack.id);
            assert!(takes(ack, &bank).contains(&take));
            recent.push(ack.id.clone());
            if recent.len() > 2 {
                recent.remove(0);
            }
        }
        // The decider's retired line plays its replacement, or its own old takes until the replacement has some.
        let mut store = BTreeMap::new();
        store.insert("en_okay".to_string(), vec![30]);
        assert_eq!(decider_takes("en_okay", &store).map(|(a, t)| (a.id.as_str(), t)), Some(("en_okay", vec![30])));
        store.insert("en_right_away".to_string(), vec![31]);
        assert_eq!(decider_takes("en_okay", &store).map(|(a, t)| (a.id.as_str(), t)), Some(("en_right_away", vec![31])));
        assert!(decider_takes("en_okay_stopping", &store).is_none() && decider_takes("en_dance", &store).is_none());
        // Variants and retired lines are never the rule's, even with takes.
        let mut only_special = BTreeMap::new();
        only_special.insert("en_right_away__urgent".to_string(), vec![20]);
        only_special.insert("en_okay".to_string(), vec![21]);
        assert!(choose("en", Pool::Action, &only_special, &[], 0).is_none());
        // A Polish turn gets no ack, even with last night's Polish takes still in the user's store.
        bank.insert("pl_sprawdzam".into(), vec![9]);
        for pool in [Pool::Action, Pool::Question, Pool::Look, Pool::Note] {
            assert!(choose("pl", pool, &bank, &[], 0).is_none());
        }
    }
}
