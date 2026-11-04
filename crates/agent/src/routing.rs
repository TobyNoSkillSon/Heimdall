//! Which T3 account the relay runs on (docs/claude-relay.md, "Accounts"). with several
//! Claude (and Codex) subscriptions in T3, pick among the eligible accounts the one whose quota resets soonest, weekly
//! and five-hour windows both counted; stay on it while it works; when it fails, the same model on another account of
//! the same provider first, then (the main assistant only) another provider. Pure: the catalog is T3's
//! `orchestrator_capabilities`, the quotas are what `Quotas` read live (none: unknown, never "unused").
//!
//! The ranking rule (the exact order when the two windows disagree was not settled with the user; this is the documented
//! choice, Assistant/Plans/relay-account-routing-2026-10-30.md):
//! 1. Not eligible: disabled, no full access, no model, the failed account, or exhausted (a known window at 100%, or
//!    the provider says a limit is reached). An exhausted account never wins by resetting soonest.
//! 2. Accounts with a known quota before unknown ones (unknown is not headroom).
//! 3. A known account with its five-hour window at 95% or more after the others: it would fail within minutes.
//! 4. The earliest weekly reset first (the user put weekly first), compared to the hour so minutes don't decide.
//! 5. Then the earliest five-hour reset, to the quarter hour.
//! 6. Then T3's order.
use crate::emergency::{shares_subscription, Harness};
use serde_json::Value;
use std::collections::HashMap;

/// One quota window: how much is used and when it resets, each when the provider said.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window { pub used_pct: Option<f64>, pub reset_ms: Option<u64> }
/// One account's quota as read live (`crate::accounts`, `Tools/subscription_quota.py`): its windows, whether the provider says
/// a limit is reached, or why it could not be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quota { pub five_hour: Window, pub weekly: Window, pub reached: bool, pub error: Option<String> }
impl Quota {
    /// Known enough to rank on: read without error, with at least one window's use.
    pub fn known(&self) -> bool { self.error.is_none() && (self.five_hour.used_pct.is_some() || self.weekly.used_pct.is_some()) }
    pub fn exhausted(&self) -> bool {
        self.reached || [&self.five_hour, &self.weekly].iter().any(|w| w.used_pct.is_some_and(|u| u >= 100.0))
    }
    /// When an exhausted account can answer again: the latest reset among its spent windows.
    pub fn back_ms(&self) -> Option<u64> {
        [&self.five_hour, &self.weekly].iter().filter(|w| w.used_pct.is_some_and(|u| u >= 100.0)).filter_map(|w| w.reset_ms).max()
            .or(if self.reached { self.five_hour.reset_ms.or(self.weekly.reset_ms) } else { None })
    }
    /// One short line for a choice card: "5-hour 40% · weekly 72% · resets Sun 14:00", or that it is unknown.
    pub fn line(&self, clock: &dyn Fn(u64) -> String) -> String {
        if !self.known() { return "quota unknown".into(); }
        let pct = |n: &str, w: &Window| w.used_pct.map(|u| format!("{n} {}%", u.round() as i64));
        let mut parts: Vec<String> = [pct("5-hour", &self.five_hour), pct("weekly", &self.weekly)].into_iter().flatten().collect();
        if let Some(r) = self.weekly.reset_ms { parts.push(format!("week resets {}", clock(r))); }
        parts.join(" · ")
    }
}
/// The live quotas by T3 provider instance id.
pub type Quotas = HashMap<String, Quota>;

/// The shared reading (`crate::accounts`, one per process with the background jobs' fallback) as this rule's quotas:
/// each account's "5-hour" and "weekly" windows, Codex's reached limit, a failed read as unknown.
pub fn quotas_from(accounts: &[crate::accounts::AccountQuota]) -> Quotas {
    let window = |a: &crate::accounts::AccountQuota, name: &str| a.windows.iter().find(|w| w.name == name)
        .map_or_else(Window::default, |w| Window { used_pct: w.used_percent, reset_ms: w.resets_at_ms });
    accounts.iter().map(|a| (a.instance.clone(), Quota {
        five_hour: window(a, "5-hour"),
        weekly: window(a, "weekly"),
        reached: a.reached.as_deref().is_some_and(|r| !r.is_empty()),
        error: a.error.clone(),
    })).collect()
}

/// What failed, for the failover order: the account (instance), its provider (driver kind) and model; or Pi's API
/// provider (`pi_provider`, e.g. ChatGPT) when the built-in agent failed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Failed { pub instance: Option<String>, pub driver: Option<String>, pub model: Option<String>, pub pi_provider: Option<String> }

struct Candidate { order: usize, harness: Harness, models: Vec<String>, quota: Option<Quota> }

/// Every instance T3 lists that can run at all, in T3's order, with its live quota.
fn candidates(catalog: &Value, quotas: &Quotas) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (order, p) in catalog["providers"].as_array().into_iter().flatten().enumerate() {
        let disabled = p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.to_ascii_lowercase().contains("disabled")));
        let full = p["runtimeModes"].as_array().is_none_or(|m| m.iter().any(|x| x == "full-access"));
        let models: Vec<String> = p["models"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect();
        let Some(instance) = p["providerInstanceId"].as_str() else { continue };
        if disabled || !full || models.is_empty() { continue; }
        let driver = p["driverKind"].as_str().unwrap_or(instance).to_owned();
        let harness = Harness { instance: instance.into(), name: p["displayName"].as_str().unwrap_or(instance).into(), driver, model: models[0].clone() };
        out.push(Candidate { order, harness, models, quota: quotas.get(instance).cloned() });
    }
    out
}
/// The rule's sort key (lower first); `None`: not eligible.
fn key(c: &Candidate) -> Option<(u8, u8, u64, u64, usize)> {
    const HOUR: u64 = 3_600_000;
    const QUARTER: u64 = 15 * 60_000;
    match &c.quota {
        Some(q) if q.exhausted() => None,
        Some(q) if q.known() => {
            let tight = q.five_hour.used_pct.is_some_and(|u| u >= 95.0) as u8;
            Some((0, tight, q.weekly.reset_ms.map_or(u64::MAX, |r| r / HOUR), q.five_hour.reset_ms.map_or(u64::MAX, |r| r / QUARTER), c.order))
        }
        _ => Some((1, 0, u64::MAX, u64::MAX, c.order)),
    }
}
fn ranked(mut list: Vec<Candidate>) -> Vec<Candidate> {
    list.retain(|c| key(c).is_some());
    list.sort_by_key(|c| key(c).unwrap());
    list
}
/// Eligible candidates in the edition's driver order, each driver's accounts ranked by the rule.
fn by_order(list: Vec<Candidate>, order: &[String]) -> Vec<Candidate> {
    let mut list: Vec<(usize, Candidate)> = list.into_iter().filter(|c| key(c).is_some()).filter_map(|c| order_rank(&c.harness.driver, order).map(|r| (r, c))).collect();
    list.sort_by_key(|(r, c)| (*r, key(c).unwrap()));
    list.into_iter().map(|(_, c)| c).collect()
}
fn kind(driver: &str) -> String { driver.to_ascii_lowercase() }
/// Whether `driver` passes the edition's order (`emergency::default_order`, `Settings::relay_order`), and its place.
fn order_rank(driver: &str, order: &[String]) -> Option<usize> {
    let lower = kind(driver);
    if order.iter().filter_map(|o| o.strip_prefix('-')).any(|n| lower.contains(&n.to_ascii_lowercase())) { return None; }
    let wanted: Vec<String> = order.iter().filter(|o| !o.starts_with('-')).map(|o| o.to_ascii_lowercase()).collect();
    wanted.iter().position(|o| o != "*" && lower.contains(o.as_str())).or(wanted.iter().position(|o| o == "*"))
}
fn with_model(mut c: Candidate, model: Option<&str>) -> Harness {
    if let Some(m) = model.filter(|m| c.models.iter().any(|x| x == m)) { c.harness.model = m.into(); }
    c.harness
}

/// Relay mode starting (no failure): the account the user picked first (`account`), else every account of the first driver
/// in `order` that runs (Claude), ranked by the rule; then the other drivers in `order`, each ranked. Each runs
/// `model` when its instance lists it.
pub fn initial(catalog: &Value, quotas: &Quotas, order: &[String], model: Option<&str>, account: Option<&str>) -> Vec<Harness> {
    let mut all = by_order(candidates(catalog, quotas), order);
    // The account the user chose leads even when its quota is unknown; when it is exhausted it is not there (the failover
    // takes over from there).
    if let Some(i) = account.and_then(|a| all.iter().position(|c| c.harness.instance == a)) {
        let chosen = all.remove(i);
        all.insert(0, chosen);
    }
    all.into_iter().map(|c| with_model(c, model)).collect()
}

/// After a failure: the same model on the other accounts of the failed provider first, ranked; then, for the main
/// assistant only (`cross_provider`), every other provider in `order`, each ranked, with its own first model unless it
/// lists the same one. An account on the failed Pi provider's subscription (ChatGPT ↔ Codex) is left out unless its
/// own quota reads usable: it may be another account, or another window (bug note 2026-11-02T14-47-39: Codex at 5-hour
/// 0%, weekly 74% was dropped from every card).
pub fn failover(catalog: &Value, quotas: &Quotas, failed: &Failed, order: &[String], cross_provider: bool) -> Vec<Harness> {
    let all: Vec<Candidate> = candidates(catalog, quotas).into_iter()
        .filter(|c| failed.instance.as_deref() != Some(c.harness.instance.as_str()))
        .filter(|c| failed.pi_provider.as_deref().is_none_or(|p| !shares_subscription(p, &c.harness.driver) || c.quota.as_ref().is_some_and(|q| q.known() && !q.exhausted())))
        .collect();
    let same = |c: &Candidate| failed.driver.as_deref().is_some_and(|d| kind(d) == kind(&c.harness.driver));
    let (first, rest): (Vec<Candidate>, Vec<Candidate>) = all.into_iter().partition(|c| same(c));
    let mut out: Vec<Harness> = ranked(first).into_iter().map(|c| with_model(c, failed.model.as_deref())).collect();
    if !cross_provider && failed.driver.is_some() { return out; }
    out.extend(by_order(rest, order).into_iter().map(|c| with_model(c, failed.model.as_deref())));
    out
}

/// Relay mode about to start on an account that is already spent: the user's chosen account, or (Automatic) every account
/// of the first driver in `order` (all Claude accounts spent). That is a failure like any other (the user, 06:30: never a
/// silent skip): the recovery card offers what can take over. `None`: something of the first driver can run.
pub fn spent_choice(catalog: &Value, quotas: &Quotas, order: &[String], model: Option<&str>, account: Option<&str>) -> Option<Harness> {
    let all = candidates(catalog, quotas);
    let spent = |c: &Candidate| c.quota.as_ref().is_some_and(Quota::exhausted);
    if let Some(a) = account {
        return all.into_iter().find(|c| c.harness.instance == a && spent(c)).map(|c| with_model(c, model));
    }
    let first = order.iter().find(|o| !o.starts_with('-') && *o != "*")?.to_ascii_lowercase();
    let group: Vec<Candidate> = all.into_iter().filter(|c| kind(&c.harness.driver).contains(&first)).collect();
    if group.is_empty() || !group.iter().all(spent) { return None; }
    group.into_iter().next().map(|c| with_model(c, model))
}

/// Whether the original account answers again by its quota: read without error, nothing spent. The reset passing is
/// not enough on its own (the user: "reset time alone is not proof").
pub fn recovered(quota: Option<&Quota>) -> bool {
    quota.is_some_and(|q| q.known() && !q.exhausted())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const H: u64 = 3_600_000;
    fn catalog() -> Value {
        json!({"providers":[
            {"providerInstanceId":"claudeAgent","driverKind":"claudeAgent","displayName":"Claude","models":[{"id":"claude-opus-5-5"},{"id":"claude-sonnet-5-5"}]},
            {"providerInstanceId":"claudeAgent_claude_code_2","driverKind":"claudeAgent","displayName":"Claude code 2","models":[{"id":"claude-opus-5-5"},{"id":"claude-sonnet-5-5"}]},
            {"providerInstanceId":"codex","driverKind":"codex","displayName":"Codex","models":[{"id":"gpt-6"}]},
            {"providerInstanceId":"grok","driverKind":"grok","displayName":"Grok","models":[{"id":"grok-5"}]},
            {"providerInstanceId":"off","driverKind":"claudeAgent","displayName":"Old","models":[{"id":"claude-opus-5-5"}],"constraints":["Provider instance is disabled."]}
        ]})
    }
    fn q(five: f64, five_reset: u64, week: f64, week_reset: u64) -> Quota {
        Quota { five_hour: Window { used_pct: Some(five), reset_ms: Some(five_reset) }, weekly: Window { used_pct: Some(week), reset_ms: Some(week_reset) }, reached: false, error: None }
    }
    fn names(h: &[Harness]) -> Vec<String> { h.iter().map(|h| format!("{}/{}", h.instance, h.model)).collect() }
    fn order() -> Vec<String> { ["claude", "codex", "*"].iter().map(|s| s.to_string()).collect() }

    #[test]
    fn the_soonest_weekly_reset_wins_then_the_five_hour_one() {
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent".into(), q(10.0, 3 * H, 50.0, 100 * H));
        quotas.insert("claudeAgent_claude_code_2".into(), q(80.0, 1 * H, 90.0, 20 * H));
        // Account 2's week ends sooner: its quota would go unused.
        let h = initial(&catalog(), &quotas, &order(), Some("claude-opus-5-5"), None);
        assert_eq!(names(&h)[..2], ["claudeAgent_claude_code_2/claude-opus-5-5", "claudeAgent/claude-opus-5-5"]);
        // The same weekly hour: the five-hour reset decides.
        quotas.insert("claudeAgent".into(), q(10.0, 30 * 60_000, 50.0, 20 * H + 600_000));
        let h = initial(&catalog(), &quotas, &order(), None, None);
        assert_eq!(h[0].instance, "claudeAgent");
        // Then the other providers, in the edition's order; disabled instances never.
        assert_eq!(h.iter().map(|h| h.instance.as_str()).collect::<Vec<_>>(), ["claudeAgent", "claudeAgent_claude_code_2", "codex", "grok"]);
    }

    #[test]
    fn exhausted_never_wins_unknown_comes_after_known_and_a_tight_window_goes_last() {
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent".into(), q(100.0, H, 40.0, 2 * H));
        quotas.insert("claudeAgent_claude_code_2".into(), q(20.0, 4 * H, 30.0, 150 * H));
        let h = initial(&catalog(), &quotas, &order(), None, None);
        assert_eq!(h[0].instance, "claudeAgent_claude_code_2", "spent, though it resets soonest");
        assert!(!h.iter().any(|h| h.instance == "claudeAgent"));
        // The provider says the limit is reached: out, whatever the numbers.
        let mut reached = q(10.0, H, 10.0, 2 * H);
        reached.reached = true;
        quotas.insert("claudeAgent".into(), reached);
        assert!(!initial(&catalog(), &quotas, &order(), None, None).iter().any(|h| h.instance == "claudeAgent"));
        // Unknown (unreadable) after a known account with a later reset.
        quotas.insert("claudeAgent".into(), Quota { error: Some("no get_usage reply".into()), ..Quota::default() });
        assert_eq!(initial(&catalog(), &quotas, &order(), None, None)[0].instance, "claudeAgent_claude_code_2");
        // Five-hour at 97% with the soonest week: after the account with headroom.
        quotas.insert("claudeAgent".into(), q(97.0, H, 10.0, 2 * H));
        assert_eq!(initial(&catalog(), &quotas, &order(), None, None)[0].instance, "claudeAgent_claude_code_2");
        // Nothing known at all: T3's order.
        assert_eq!(initial(&catalog(), &Quotas::new(), &order(), None, None)[0].instance, "claudeAgent");
    }

    #[test]
    fn the_account_owner_picked_leads_unless_it_is_spent() {
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent".into(), q(10.0, H, 10.0, 2 * H));
        let h = initial(&catalog(), &quotas, &order(), None, Some("claudeAgent_claude_code_2"));
        assert_eq!(h[0].instance, "claudeAgent_claude_code_2", "unknown quota, but his choice");
        quotas.insert("claudeAgent_claude_code_2".into(), q(100.0, H, 10.0, 2 * H));
        assert_eq!(initial(&catalog(), &quotas, &order(), None, Some("claudeAgent_claude_code_2"))[0].instance, "claudeAgent");
    }

    #[test]
    fn failover_keeps_the_model_on_the_other_account_first_then_other_providers_for_the_main_assistant_only() {
        let failed = Failed { instance: Some("claudeAgent".into()), driver: Some("claudeAgent".into()), model: Some("claude-sonnet-5-5".into()), pi_provider: None };
        let h = failover(&catalog(), &Quotas::new(), &failed, &order(), true);
        assert_eq!(names(&h), ["claudeAgent_claude_code_2/claude-sonnet-5-5", "codex/gpt-6", "grok/grok-5"]);
        assert_eq!(names(&failover(&catalog(), &Quotas::new(), &failed, &order(), false)), ["claudeAgent_claude_code_2/claude-sonnet-5-5"], "no other provider");
        // The other Claude account spent too: straight to the next provider.
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent_claude_code_2".into(), q(100.0, H, 50.0, 9 * H));
        assert_eq!(names(&failover(&catalog(), &quotas, &failed, &order(), true)), ["codex/gpt-6", "grok/grok-5"]);
        // Codex fails: Codex accounts first (none left), then Claude, ranked.
        let codex = Failed { instance: Some("codex".into()), driver: Some("codex".into()), model: Some("gpt-6".into()), pi_provider: None };
        let h = failover(&catalog(), &quotas, &codex, &order(), true);
        assert_eq!(h.iter().map(|h| h.instance.as_str()).collect::<Vec<_>>(), ["claudeAgent", "grok"]);
        // The personal edition's order may still leave a provider out.
        let personal: Vec<String> = ["claude", "*", "-grok"].iter().map(|s| s.to_string()).collect();
        assert!(!failover(&catalog(), &Quotas::new(), &failed, &personal, true).iter().any(|h| h.instance == "grok"));
    }

    #[test]
    fn pi_failing_on_chatgpt_skips_codex_and_ranks_the_claude_accounts() {
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent".into(), q(10.0, H, 10.0, 90 * H));
        quotas.insert("claudeAgent_claude_code_2".into(), q(10.0, H, 10.0, 30 * H));
        let pi = Failed { pi_provider: Some(crate::provider::CHATGPT.into()), ..Failed::default() };
        let h = failover(&catalog(), &quotas, &pi, &order(), true);
        assert_eq!(h.iter().map(|h| h.instance.as_str()).collect::<Vec<_>>(), ["claudeAgent_claude_code_2", "claudeAgent", "grok"]);
        // Bug note 2026-11-02T14-47-39: Codex's own quota reads usable (5-hour 0%, weekly 74%): it is offered.
        quotas.insert("codex".into(), q(0.0, H, 74.0, 50 * H));
        let h = failover(&catalog(), &quotas, &pi, &order(), true);
        assert_eq!(h.iter().map(|h| h.instance.as_str()).collect::<Vec<_>>(), ["claudeAgent_claude_code_2", "claudeAgent", "codex", "grok"]);
        // Spent with the same plan: out, as before.
        quotas.insert("codex".into(), q(100.0, H, 74.0, 50 * H));
        assert!(!failover(&catalog(), &quotas, &pi, &order(), true).iter().any(|h| h.instance == "codex"));
    }

    #[test]
    fn a_spent_chosen_account_or_every_claude_account_spent_is_a_failure_not_a_skip() {
        let mut quotas = Quotas::new();
        quotas.insert("claudeAgent_claude_code_2".into(), q(100.0, H, 10.0, 2 * H));
        let h = spent_choice(&catalog(), &quotas, &order(), Some("claude-sonnet-5-5"), Some("claudeAgent_claude_code_2")).unwrap();
        assert_eq!((h.instance.as_str(), h.model.as_str()), ("claudeAgent_claude_code_2", "claude-sonnet-5-5"));
        assert!(spent_choice(&catalog(), &quotas, &order(), None, None).is_none(), "Automatic: the other Claude account runs");
        quotas.insert("claudeAgent".into(), q(10.0, H, 100.0, 9 * H));
        assert_eq!(spent_choice(&catalog(), &quotas, &order(), None, None).unwrap().instance, "claudeAgent", "every Claude account spent");
        assert!(spent_choice(&catalog(), &Quotas::new(), &order(), None, Some("claudeAgent")).is_none(), "unknown is not spent");
    }

    #[test]
    fn recovery_needs_a_live_read_with_nothing_spent() {
        assert!(!recovered(None));
        assert!(!recovered(Some(&Quota { error: Some("timeout".into()), ..Quota::default() })));
        assert!(!recovered(Some(&q(100.0, H, 10.0, 2 * H))));
        assert!(recovered(Some(&q(3.0, 5 * H, 60.0, 50 * H))));
        assert_eq!(q(100.0, H, 100.0, 9 * H).back_ms(), Some(9 * H));
        assert_eq!(q(10.0, 2 * H, 30.0, 9 * H).line(&|ms| format!("@{}", ms / H)), "5-hour 10% · weekly 30% · week resets @9");
        assert_eq!(Quota::default().line(&|_| String::new()), "quota unknown");
    }

    #[test]
    fn the_shared_reading_becomes_this_rule_s_quotas() {
        use crate::accounts::{AccountQuota, Kind, Window as W};
        let w = |name: &str, used: f64, at: u64| W { name: name.into(), used_percent: Some(used), resets_at_ms: Some(at) };
        let q = quotas_from(&[
            AccountQuota { instance: "claudeAgent".into(), kind: Kind::Claude, windows: vec![w("5-hour", 40.0, 1), w("weekly", 100.0, 2)], reached: None, error: None },
            AccountQuota { instance: "codex".into(), kind: Kind::Codex, windows: vec![w("5-hour", 10.0, 3)], reached: Some("primary".into()), error: None },
            AccountQuota { instance: "claudeAgent_claude_code_2".into(), kind: Kind::Claude, windows: vec![], reached: None, error: Some("no reply".into()) },
        ]);
        let c = &q["claudeAgent"];
        assert_eq!((c.five_hour.used_pct, c.weekly.reset_ms), (Some(40.0), Some(2)));
        assert!(c.exhausted() && c.known());
        assert!(q["codex"].exhausted() && q["codex"].weekly == Window::default());
        assert!(!q["claudeAgent_claude_code_2"].known() && !q["claudeAgent_claude_code_2"].exhausted(), "a failed read is unknown, not spent");
    }

}
