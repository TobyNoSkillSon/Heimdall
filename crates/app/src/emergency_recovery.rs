//! The recovery cards (docs/claude-relay.md, "Accounts and recovery";). When the agent's
//! account fails, the pull-down shows what can take over, found by a quick scan (T3's catalog and each account's live
//! quota): other accounts and providers, the local model when it is on this Mac, and waiting for the original, with a
//! one-minute countdown; unanswered, Heimdall searches by itself (`agent::routing::failover`). When the original works
//! again, one question offers to go back; silence keeps the replacement. Pure: `emergency.rs` shows and answers them.
use agent::emergency::Harness;
use agent::routing::Quotas;
use ui::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};

/// Request id prefixes: the answers reach the controller by these (`pipeline/actions.rs`).
pub const PREFIX: &str = "relay-recovery:";
pub const RETURN_PREFIX: &str = "relay-return:";
/// How long the user has to pick before Heimdall searches by itself.
pub const COUNTDOWN_MS: u64 = if cfg!(test) { 1_500 } else { 60_000 };
/// The return question stays open this long; unanswered, the replacement stays.
pub const RETURN_MS: u64 = 4 * 60 * 60_000;
/// A card holds four options: the wait and the local model take their places, accounts fill the rest.
const OPTIONS: usize = 4;
const LABEL_CHARS: usize = agent::mcp_contract::LABEL_CHARS;

/// Where the agent runs: an account (T3 provider instance) and model. Serialised in the controller's state.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Route { pub instance: String, pub driver: String, pub name: String, pub model: String }
impl From<&Harness> for Route {
    fn from(h: &Harness) -> Self { Self { instance: h.instance.clone(), driver: h.driver.clone(), name: h.name.clone(), model: h.model.clone() } }
}
impl Route {
    pub fn harness(&self) -> Harness { Harness { instance: self.instance.clone(), driver: self.driver.clone(), name: self.name.clone(), model: self.model.clone() } }
    /// "Claude code 2 · Opus 5.5".
    pub fn label(&self) -> String { bounded(&format!("{} · {}", self.name, model_name(&self.model))) }
}
/// What the user had before the failure, to offer going back to: the built-in agent on its model, or a relay account.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Original { Pi { model: Option<agent::provider::ApiModel> }, Relay(Route) }
impl Original {
    pub fn label(&self) -> String {
        match self {
            Self::Pi { model: Some(m) } => bounded(&format!("{} · built in", m.name)),
            Self::Pi { model: None } => "the built-in agent".into(),
            Self::Relay(r) => r.label(),
        }
    }
}
/// One choice on the recovery card.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Pick { Route(Route), Local { model: String }, Wait }
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Choice { pub label: String, pub description: Option<String>, pub pick: Pick }
/// The open recovery card: its id, when the search starts by itself, its choices, and the search order.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Open { pub request_id: String, pub until_ms: u64, pub choices: Vec<Choice>, pub order: Vec<Route> }

/// "claude-opus-5-5" → "Opus 5.5"; other ids as they are.
pub fn model_name(id: &str) -> String {
    if let Some(rest) = id.strip_prefix("claude-") {
        let mut parts = rest.split('-');
        if let Some(family) = parts.next() {
            let version: Vec<&str> = parts.collect();
            let mut f = family.chars();
            let family = f.next().map(|c| c.to_uppercase().collect::<String>() + f.as_str()).unwrap_or_default();
            return if version.is_empty() { family } else { format!("{family} {}", version.join(".")) };
        }
    }
    id.into()
}
fn bounded(s: &str) -> String {
    if s.chars().count() <= LABEL_CHARS { s.into() } else { s.chars().take(LABEL_CHARS - 1).collect::<String>() + "…" }
}

/// The local engine as the quick scan finds it (`emergency_local::engine`, no network): ready, or its model still to
/// download (bug note 2026-11-02T14-47-39: it was offered only when ready, and its executable was never found).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local { pub model: String, pub label: String, pub ready: bool, pub download_bytes: Option<u64> }

/// Which accounts of the search order get the card's places (`slots`): the failed provider's other accounts first
/// (the same model elsewhere), then the first account of every other provider, then the rest; in the search order.
/// Everything stays in the automatic search either way (bug note 2026-11-02T14-47-39: offer all that works).
fn shown(order: &[Harness], failed_driver: Option<&str>, slots: usize) -> Vec<usize> {
    let kind = |h: &Harness| h.driver.to_ascii_lowercase();
    let failed = failed_driver.map(str::to_ascii_lowercase);
    let mut picked: Vec<usize> = (0..order.len()).filter(|&i| failed.as_deref() == Some(kind(&order[i]).as_str())).take(slots).collect();
    let mut drivers: Vec<String> = failed.into_iter().collect();
    for (i, h) in order.iter().enumerate() {
        if picked.len() >= slots { break; }
        if !drivers.contains(&kind(h)) { drivers.push(kind(h)); picked.push(i); }
    }
    for i in 0..order.len() {
        if picked.len() >= slots { break; }
        if !picked.contains(&i) { picked.push(i); }
    }
    picked.sort_unstable();
    picked
}

/// The card's choices: accounts and providers from the search order (each with its quota, or that it is unknown), the
/// local model when the engine is on this Mac, and waiting for the original (with its reset when known). Two to four.
pub fn choices(order: &[Harness], failed_driver: Option<&str>, quotas: &Quotas, local: Option<Local>, original: &Original, back_ms: Option<u64>, clock: &dyn Fn(u64) -> String) -> Vec<Choice> {
    let fixed = 1 + local.is_some() as usize;
    let mut out: Vec<Choice> = shown(order, failed_driver, OPTIONS - fixed).into_iter().map(|i| {
        let h = &order[i];
        let route = Route::from(h);
        let quota = quotas.get(&h.instance).map_or_else(|| "quota unknown".to_owned(), |q| q.line(clock));
        Choice { label: route.label(), description: Some(bounded_description(&quota)), pick: Pick::Route(route) }
    }).collect();
    if let Some(l) = local {
        let note = if l.ready { "on this Mac".to_owned() } else { l.download_bytes.map_or_else(|| "downloads first".to_owned(), |b| format!("{} download first", crate::emergency_local::size_text(b))) };
        out.push(Choice { label: bounded(&format!("Local · {}", l.label)), description: Some(note), pick: Pick::Local { model: l.model } });
    }
    let wait = format!("Wait for {}", original.label());
    out.push(Choice { label: bounded(&wait), description: back_ms.map(|t| format!("back at {}", clock(t))), pick: Pick::Wait });
    out
}
fn bounded_description(s: &str) -> String { s.chars().take(agent::mcp_contract::DESCRIPTION_CHARS).collect() }

/// The recovery card, its countdown running to `until_ms`.
pub fn card(request_id: &str, why: &str, choices: &[Choice], until_ms: u64) -> AgentNotice {
    let question = bounded_question(&format!("{why} Switch to?"));
    AgentNotice {
        id: request_id.into(),
        text: question.clone(),
        kind: NoticeKind::Warning,
        action: Some(NoticeAction::Ask {
            request_id: request_id.into(),
            questions: vec![AskQuestion { id: "q1".into(), header: None, text: question, multi_select: false, allow_other: false, field: None,
                options: choices.iter().map(|c| AskOption { label: c.label.clone(), description: c.description.clone(), grant: vec![] }).collect() }],
            expires_ms: until_ms,
            countdown: true,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    }
}
fn bounded_question(s: &str) -> String { s.chars().take(agent::mcp_contract::QUESTION_CHARS).collect() }
/// The return question's two answers.
pub fn back_label(original: &Original) -> String { bounded(&format!("Back to {}", original.label())) }
pub fn stay_label(current: &Route) -> String { bounded(&format!("Stay on {}", current.label())) }
/// "Claude is back. Go back to it?" with Back / Stay.
pub fn return_card(request_id: &str, original: &Original, current: &Route, now_ms: u64) -> AgentNotice {
    let question = bounded_question(&format!("{} works again. Go back?", original.label()));
    AgentNotice {
        id: request_id.into(),
        text: question.clone(),
        kind: NoticeKind::Info,
        action: Some(NoticeAction::Ask {
            request_id: request_id.into(),
            questions: vec![AskQuestion { id: "q1".into(), header: None, text: question, multi_select: false, allow_other: false, field: None,
                options: vec![AskOption::label(&back_label(original)), AskOption::label(&stay_label(current))] }],
            expires_ms: now_ms + RETURN_MS,
            countdown: false,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    }
}
/// The original as said aloud: "Opus 5.5 on Claude", "your built-in agent".
pub fn spoken(original: &Original) -> String {
    match original {
        Original::Pi { model: Some(m) } => format!("your built-in agent on {}", m.name),
        Original::Pi { model: None } => "your built-in agent".into(),
        Original::Relay(r) => format!("{} on {}", model_name(&r.model), r.name),
    }
}
/// A route as said aloud: "Opus 5.5 on Claude code 2".
pub fn spoken_route(r: &Route) -> String { format!("{} on {}", model_name(&r.model), r.name) }

/// What Heimdall says when the recovery card opens (bug note 2026-11-02T14-47-46: the switches were never announced):
/// what failed and until when, and what happens when the minute runs out. Spoken, or shown on the island with
/// Speaking off (`mcp_actions::say`).
pub fn card_line(original: &Original, quota: bool, back: Option<&str>, choices: &[Choice]) -> String {
    let who = spoken(original);
    let mut first = who.chars();
    let who = first.next().map(|c| c.to_uppercase().collect::<String>() + first.as_str()).unwrap_or_default();
    let what = match (quota, back) {
        (true, Some(t)) => format!("{who} is out of quota until {t}."),
        (true, None) => format!("{who} is out of quota."),
        (false, _) => format!("{who} can't answer."),
    };
    match choices.iter().find_map(|c| match &c.pick { Pick::Route(r) => Some(r), _ => None }) {
        Some(r) => format!("{what} In a minute I'll switch to {}, unless you pick something else on the card.", spoken_route(r)),
        None => format!("{what} Nothing else can take over by itself; the card has what's left."),
    }
}
/// The island's short notice with the card: "Claude · Opus 5.5 out till 13:00 · switching in a minute".
pub fn card_notice(original: &Original, quota: bool, back: Option<&str>, any_route: bool) -> String {
    let state = match (quota, back) { (true, Some(t)) => format!("out till {t}"), (true, None) => "out of quota".into(), (false, _) => "can't answer".into() };
    let then = if any_route { "switching in a minute" } else { "pick on the card" };
    format!("{} {state} · {then}", original.label())
}
/// What Heimdall says once a switch took over: "Switched to Opus 5.5 on Claude code 2."
pub fn switched_line(r: &Route) -> String { format!("Switched to {}.", spoken_route(r)) }

/// What Heimdall says aloud with the return card.
pub fn return_line(original: &Original) -> String {
    format!("{} works again. Do you want to go back to it, or stay where we are?", match original {
        Original::Pi { .. } => "Your built-in agent".to_owned(),
        Original::Relay(r) => format!("{} on {}", model_name(&r.model), r.name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::routing::{Quota, Window};

    fn h(instance: &str, name: &str, model: &str) -> Harness { Harness { instance: instance.into(), driver: "claudeAgent".into(), name: name.into(), model: model.into() } }

    #[test]
    fn the_card_offers_what_is_there_with_honest_quotas_and_always_the_wait() {
        let order = [h("claudeAgent_claude_code_2", "Claude code 2", "claude-opus-5-5"), h("codex", "Codex", "gpt-6"), h("grok", "Grok", "grok-5")];
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent_claude_code_2".into(), Quota { five_hour: Window { used_pct: Some(25.0), reset_ms: None }, weekly: Window { used_pct: Some(21.0), reset_ms: None }, ..Quota::default() });
        let original = Original::Relay(Route::from(&h("claudeAgent", "Claude", "claude-opus-5-5")));
        let c = choices(&order, Some("claudeAgent"), &quotas, Some(Local { model: "qwen".into(), label: "Qwen3.8 27B".into(), ready: true, download_bytes: None }), &original, Some(0), &|_| "14:00".into());
        let labels: Vec<&str> = c.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["Claude code 2 · Opus 5.5", "Codex · gpt-6", "Local · Qwen3.8 27B", "Wait for Claude · Opus 5.5"]);
        assert_eq!(c[0].description.as_deref(), Some("5-hour 25% · weekly 21%"));
        assert_eq!(c[1].description.as_deref(), Some("quota unknown"), "never claimed as headroom");
        assert_eq!(c[3].description.as_deref(), Some("back at 14:00"));
        // No local model, one account: two choices still (a card needs two).
        let c = choices(&order[..1], None, &Quotas::new(), None, &Original::Pi { model: None }, None, &|_| String::new());
        assert_eq!(c.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Claude code 2 · Opus 5.5", "Wait for the built-in agent"]);
        let card = card("relay-recovery:1", "Claude's quota ran out.", &c, 60_000);
        let Some(NoticeAction::Ask { countdown, expires_ms, questions, .. }) = card.action else { panic!() };
        assert!(countdown && expires_ms == 60_000 && questions[0].text == "Claude's quota ran out. Switch to?");
    }

    /// Bug note 2026-11-02T14-47-39: the card offers one of every provider that can take over after the failed
    /// provider's other accounts, and the local model even when it still has to download (with what that takes).
    #[test]
    fn the_card_offers_every_provider_and_the_local_model_still_to_download() {
        let g = |i: &str, d: &str| Harness { instance: i.into(), driver: d.into(), name: i.into(), model: "m".into() };
        let order = [g("claude2", "claudeAgent"), g("claude3", "claudeAgent"), g("codex", "codex"), g("grok", "grok")];
        let local = Local { model: "qwen".into(), label: "Qwen3.8 27B".into(), ready: false, download_bytes: Some(17_400_000_000) };
        let c = choices(&order, None, &Quotas::new(), Some(local.clone()), &Original::Pi { model: None }, None, &|_| String::new());
        assert_eq!(c.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["claude2 · m", "codex · m", "Local · Qwen3.8 27B", "Wait for the built-in agent"]);
        assert_eq!(c[2].description.as_deref(), Some("17.4 GB download first"));
        // The same model elsewhere first when a Claude account failed, then another provider.
        let c = choices(&order, Some("claudeAgent"), &Quotas::new(), None, &Original::Pi { model: None }, None, &|_| String::new());
        assert_eq!(c.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["claude2 · m", "claude3 · m", "codex · m", "Wait for the built-in agent"]);
        let c = choices(&order[..1], None, &Quotas::new(), Some(local), &Original::Pi { model: None }, None, &|_| String::new());
        assert_eq!(c.len(), 3);
    }

    /// Bug note 2026-11-02T14-47-46: the card and its countdown are said, and so is the switch.
    #[test]
    fn the_card_and_the_switch_are_announced() {
        let original = Original::Relay(Route::from(&h("claudeAgent", "Claude", "claude-opus-5-5")));
        let next = Route::from(&h("claudeAgent_claude_code_2", "Claude code 2", "claude-opus-5-5"));
        let c = vec![Choice { label: next.label(), description: None, pick: Pick::Route(next.clone()) }, Choice { label: "Wait".into(), description: None, pick: Pick::Wait }];
        assert_eq!(card_line(&original, true, Some("13:00"), &c), "Opus 5.5 on Claude is out of quota until 13:00. In a minute I'll switch to Opus 5.5 on Claude code 2, unless you pick something else on the card.");
        assert_eq!(card_notice(&original, true, Some("13:00"), true), "Claude · Opus 5.5 out till 13:00 · switching in a minute");
        assert_eq!(card_line(&original, false, None, &c[1..]), "Opus 5.5 on Claude can't answer. Nothing else can take over by itself; the card has what's left.");
        assert_eq!(switched_line(&next), "Switched to Opus 5.5 on Claude code 2.");
    }

    #[test]
    fn the_return_question_names_both_choices() {
        let original = Original::Relay(Route::from(&h("claudeAgent", "Claude", "claude-opus-5-5")));
        let current = Route::from(&h("claudeAgent_claude_code_2", "Claude code 2", "claude-opus-5-5"));
        let c = return_card("relay-return:1", &original, &current, 0);
        let Some(NoticeAction::Ask { questions, countdown, .. }) = c.action else { panic!() };
        assert!(!countdown);
        assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Back to Claude · Opus 5.5", "Stay on Claude code 2 · Opus 5.5"]);
        assert_eq!(return_line(&original), "Opus 5.5 on Claude works again. Do you want to go back to it, or stay where we are?");
        assert_eq!(model_name("claude-sonnet-5-5"), "Sonnet 5.5");
    }
}
