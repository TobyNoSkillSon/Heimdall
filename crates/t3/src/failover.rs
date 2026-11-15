//! Bug note 2026-11-06T12-26-36: a task whose turn fails on a Claude usage limit ("You've hit your weekly limit",
//! the 5-hour or session limit) moves to another Claude account (a T3 instance: `claudeAgent`,
//! `claudeAgent_2`) that has quota left, and resumes there with "check what's on disk, then continue".
//! At 12:08 on 6 Nov five tasks failed one after another on the main account's weekly limit while the second account
//! was at 0%; at 12:27 the relay moved them by hand (`t3_thread_configure` `modelSelection.instanceId`, then a send),
//! with the user's blessing. This is that move, made by Heimdall's task follower (`Workers::failover_step`,
//! `Workers::failover_moves`), on by default (`Workers::set_accounts`; the app's off switch is
//! `Settings::task_failover_off`).
//!
//! The pure rules, tested here:
//! - `usage_limit`: whether a turn's last words are a plan usage limit, and which window.
//! - `choose`: the account to move to: a Claude account other than the one that failed, not spent by the live
//!   reading (`agent::accounts`) nor by a limit Heimdall saw itself since (`spent`); known quota first, the most
//!   headroom first. None: nothing to move to.
//! - `may_move`: never loops. A task moves at most once per limit reset: a second move waits until the account it
//!   left has reset (`until_ms`), and an exhausted account is never a target.
use agent::accounts::{spent_until, AccountQuota, Kind};

/// How long a limit Heimdall saw on an account counts when no reading or reply says when it resets: the shortest
/// window (5 hours).
pub const DEFAULT_SPENT_MS: u64 = 5 * 60 * 60 * 1000;
/// A move whose configure or send keeps failing is reported as the failure it was after this long.
pub const GIVE_UP_MS: u64 = 10 * 60 * 1000;

/// The window a usage-limit failure names: "weekly", "5-hour", "session", or "usage" when it names none. Only a
/// turn's own last words, their first line (as the sign-in rule reads them): a progress reply that quotes a limit is
/// no failure. `None`: not a usage limit (a per-minute rate limit is not one).
pub fn usage_limit(first_line: &str) -> Option<&'static str> {
    let lower = first_line.to_ascii_lowercase().replace('\u{2019}', "'");
    let hit = ["you've hit your", "you have hit your", "you've reached your", "you have reached your"].iter().any(|p| lower.contains(p))
        && lower.contains("limit");
    // Only Claude's own wording: a generic "usage limit reached" from another API, or a reply that merely says
    // "within the 5-hour limit" or "github", is not this (night review, 6 Nov).
    let claude = lower.contains("claude") || lower.trim_start().starts_with("usage limit");
    let reached = claude
        && (lower.contains("usage limit reached") || lower.contains("usage limit has been reached") || lower.contains("usage_limit_reached"));
    let window = lower.contains("weekly limit") || lower.contains("7-day limit") || lower.contains("session limit") || lower.contains("5-hour limit");
    let window_hit = window && (lower.contains("limit reached") || lower.contains("limit has been reached"));
    if !(hit || reached || window_hit) {
        return None;
    }
    Some(if lower.contains("weekly") || lower.contains("7-day") || lower.contains("seven_day") {
        "weekly"
    } else if lower.contains("5-hour") || lower.contains("five_hour") || lower.contains("5 hour") {
        "5-hour"
    } else if lower.contains("session") {
        "session"
    } else {
        "usage"
    })
}

/// The Claude account to move a limit-failed task to, if one has quota: not `failed`, not spent in the reading, not in
/// `spent` (accounts whose limit Heimdall saw, with when it ends) before `now_ms`. Known quota before unknown (an
/// account whose read failed), then the lowest use, then the reading's order.
pub fn choose(failed: &str, accounts: &[AccountQuota], spent: &[(String, u64)], now_ms: u64) -> Option<String> {
    let mut list: Vec<(usize, &AccountQuota)> = accounts
        .iter()
        .enumerate()
        .filter(|(_, a)| a.kind == Kind::Claude && a.instance != failed)
        .filter(|(_, a)| spent_until(a, now_ms).is_none())
        .filter(|(_, a)| !spent.iter().any(|(i, until)| *i == a.instance && *until > now_ms))
        .collect();
    let used = |a: &AccountQuota| a.windows.iter().filter_map(|w| w.used_percent).fold(None, |m: Option<f64>, u| Some(m.map_or(u, |m| m.max(u))));
    list.sort_by(|(i, a), (j, b)| {
        let (ka, kb) = (a.error.is_none() && used(a).is_some(), b.error.is_none() && used(b).is_some());
        kb.cmp(&ka).then(used(a).unwrap_or(0.0).total_cmp(&used(b).unwrap_or(0.0))).then(i.cmp(j))
    });
    list.first().map(|(_, a)| a.instance.clone())
}

/// When the account that failed can answer again: the reading's reset for its spent windows, else what the reply
/// says ("resets in 72h 52m"), else `DEFAULT_SPENT_MS` from now.
pub fn back_ms(failed: &str, accounts: &[AccountQuota], text: &str, now_ms: u64) -> u64 {
    if let Some(until) = accounts.iter().find(|a| a.instance == failed).and_then(|a| spent_until(a, now_ms)).filter(|u| *u != u64::MAX) {
        return until;
    }
    resets_in(text).map_or(now_ms + DEFAULT_SPENT_MS, |ms| now_ms + ms)
}
/// "resets in 72h 52m", "resets in 4h", "resets in 35m": the time left, in ms.
fn resets_in(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let rest = &lower[lower.find("resets in ")? + "resets in ".len()..];
    let mut total = 0u64;
    let mut any = false;
    for word in rest.split_whitespace().take(3) {
        let word = word.trim_end_matches(['.', ',', ')']);
        let (n, unit) = word.split_at(word.find(|c: char| !c.is_ascii_digit()).unwrap_or(word.len()));
        let Ok(n) = n.parse::<u64>() else { break };
        total += n * match unit { "d" => 86_400_000, "h" => 3_600_000, "m" => 60_000, _ => break };
        any = true;
    }
    any.then_some(total)
}

/// Whether a thread's T3 instance is a Claude account: the reading's kind when it lists it, else T3's Claude instance
/// naming (`claudeAgent`, `claudeAgent_<name>`). A Codex thread's "You've hit your usage limit" is no Claude limit
/// (night review H2, 6 Nov): Claude's own wording names no provider, so the thread's instance decides.
pub fn is_claude(instance: &str, accounts: &[AccountQuota]) -> bool {
    accounts.iter().find(|a| a.instance == instance).map_or_else(|| instance == "claudeAgent" || instance.starts_with("claudeAgent_"), |a| a.kind == Kind::Claude)
}

/// The no-loop rule: a task that already moved (`last_until`: when the account it left resets) moves again only once
/// that account has reset.
pub fn may_move(last_until: Option<u64>, now_ms: u64) -> bool {
    last_until.is_none_or(|until| now_ms >= until)
}

/// An account's name as the user says it.
pub fn account_name(instance: &str) -> String {
    match instance {
        "claudeAgent" => "the main Claude account".into(),
        "claudeAgent_2" => "Claude code 2".into(),
        other => other.to_owned(),
    }
}

/// The message that resumes a moved task (the relay's own words at 12:26, made general).
pub fn resume_message(window: &str, from: &str, to: &str) -> String {
    let limit = if window == "usage" { "usage limit".to_owned() } else { format!("{window} limit") };
    format!(
        "Your run stopped when {} hit its {limit}. Heimdall moved this thread to {}. Resume where you stopped: check what is \
         already done on disk (files, commits, background jobs) before you redo anything, then carry on with the brief and \
         every steer you were given.",
        account_name(from),
        account_name(to)
    )
}

/// Bug note 2026-11-08T02-03-49 (01:58:55): `delegate_task` without `target.providerInstanceId`, for a model that
/// several T3 instances offer. When every candidate is a Claude account, the first in T3's order that has quota (not
/// spent in the reading `accounts` nor by a limit Heimdall saw, `spent`) is the stable default; all spent: the first.
/// `None`: some candidate is not a Claude account, so the caller has to say which (MODEL_AMBIGUOUS).
pub fn default_account(candidates: &[&str], accounts: &[AccountQuota], spent: &[(String, u64)], now_ms: u64) -> Option<String> {
    if candidates.is_empty() || !candidates.iter().all(|c| is_claude(c, accounts)) {
        return None;
    }
    let has_quota = |c: &&&str| {
        !accounts.iter().any(|a| a.instance == **c && spent_until(a, now_ms).is_some()) && !spent.iter().any(|(i, until)| i == **c && *until > now_ms)
    };
    candidates.iter().find(has_quota).or(candidates.first()).map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::accounts::Window;
    fn acct(instance: &str, weekly: Option<f64>, reset: Option<u64>) -> AccountQuota {
        AccountQuota {
            instance: instance.into(),
            kind: Kind::Claude,
            windows: vec![Window { name: "weekly".into(), used_percent: weekly, resets_at_ms: reset }],
            reached: None,
            error: None,
        }
    }
    const NOW: u64 = 1_793_964_000_000;
    const MAIN: &str = "claudeAgent";
    const TWO: &str = "claudeAgent_2";

    #[test]
    fn usage_limits_are_told_from_other_failures() {
        // The five tasks' last replies at 12:08, and T3's own notice.
        assert_eq!(usage_limit("You've hit your weekly limit · resets Nov 9 at 1pm (Europe/Warsaw)"), Some("weekly"));
        assert_eq!(usage_limit("You\u{2019}ve hit your session limit · resets 5pm"), Some("session"));
        assert_eq!(usage_limit("Claude usage limit reached. This turn is paused until the 7-day limit resets in 72h 52m."), Some("weekly"));
        assert_eq!(usage_limit("You've hit your 5-hour limit · resets 3:40pm"), Some("5-hour"));
        assert_eq!(usage_limit("Claude AI usage limit reached|1793066649"), Some("usage"));
        // Not a plan limit, or not a failure.
        for no in ["API Error: 429 rate limit, retry in 20s", "Claude Code is not signed in (401)", "Done: the weekly report is in Reports/.",
                   "Fixed the memory limit check.", "Today's notes: you've hit your goal.",
                   "Could not finish within the 5-hour limit of the job.", "Pushed to github; the weekly limit check is done.",
                   "OpenAI usage limit reached for gpt-6."] {
            assert_eq!(usage_limit(no), None, "{no}");
        }
    }

    #[test]
    fn only_a_claude_thread_has_a_claude_limit() {
        let codex = AccountQuota { instance: "codex".into(), kind: Kind::Codex, windows: vec![], reached: None, error: None };
        let live = vec![acct(MAIN, Some(100.0), None), acct(TWO, Some(0.0), None), codex];
        assert!(is_claude(MAIN, &live) && is_claude(TWO, &live) && !is_claude("codex", &live));
        // No reading (its read failed): T3's instance naming decides.
        assert!(is_claude(MAIN, &[]) && is_claude(TWO, &[]) && !is_claude("codex", &[]) && !is_claude("codex_work", &[]) && !is_claude("claudeAgentX", &[]));
    }

    #[test]
    fn the_account_with_quota_is_chosen_never_a_spent_one() {
        let reset = NOW + 3 * 86_400_000;
        // 12:26 on 6 Nov: the main account at 100% weekly, account 2 at 0%.
        let live = vec![acct(MAIN, Some(100.0), Some(reset)), acct(TWO, Some(0.0), Some(reset + 1))];
        assert_eq!(choose(MAIN, &live, &[], NOW).as_deref(), Some(TWO));
        // Back the other way once account 2 runs out and the main one has room.
        let later = vec![acct(MAIN, Some(10.0), Some(reset)), acct(TWO, Some(100.0), Some(reset))];
        assert_eq!(choose(TWO, &later, &[], NOW).as_deref(), Some(MAIN));
        // Both spent: nothing to move to.
        let both = vec![acct(MAIN, Some(100.0), Some(reset)), acct(TWO, Some(100.0), Some(reset))];
        assert_eq!(choose(MAIN, &both, &[], NOW), None);
        // A stale reading that still shows the other account free, but Heimdall saw it fail on its limit: not a target.
        assert_eq!(choose(MAIN, &live, &[(TWO.into(), NOW + 60_000)], NOW), None);
        // That record ends with its reset.
        assert_eq!(choose(MAIN, &live, &[(TWO.into(), NOW - 1)], NOW).as_deref(), Some(TWO));
        // Codex is never a target, and known quota comes before an unread account.
        let mut unknown = acct("claudeAgent_three", None, None);
        unknown.error = Some("timed out".into());
        let codex = AccountQuota { instance: "codex".into(), kind: Kind::Codex, windows: vec![], reached: None, error: None };
        assert_eq!(choose(MAIN, &[codex.clone(), unknown.clone(), acct(TWO, Some(60.0), None)], &[], NOW).as_deref(), Some(TWO));
        assert_eq!(choose(MAIN, &[codex, unknown], &[], NOW).as_deref(), Some("claudeAgent_three"));
        // A spent window whose reset has passed is quota again.
        assert_eq!(choose(MAIN, &[acct(TWO, Some(100.0), Some(NOW - 1))], &[], NOW).as_deref(), Some(TWO));
    }

    #[test]
    fn one_move_per_limit_reset_and_the_reset_is_known() {
        assert!(may_move(None, NOW));
        assert!(!may_move(Some(NOW + 1), NOW), "moved already, the account it left not reset yet");
        assert!(may_move(Some(NOW), NOW));
        let reset = NOW + 72 * 3_600_000;
        assert_eq!(back_ms(MAIN, &[acct(MAIN, Some(100.0), Some(reset))], "", NOW), reset);
        assert_eq!(back_ms(MAIN, &[], "Claude usage limit reached. This turn is paused until the 7-day limit resets in 72h 52m.", NOW), NOW + (72 * 60 + 52) * 60_000);
        assert_eq!(back_ms(MAIN, &[], "resets in 35m.", NOW), NOW + 35 * 60_000);
        assert_eq!(back_ms(MAIN, &[], "You've hit your weekly limit · resets Nov 9 at 1pm", NOW), NOW + DEFAULT_SPENT_MS);
    }

    #[test]
    fn the_resume_message_says_what_moved_and_to_check_the_disk() {
        let m = resume_message("weekly", MAIN, TWO);
        assert!(m.starts_with("Your run stopped when the main Claude account hit its weekly limit. Heimdall moved this thread to Claude code 2."), "{m}");
        assert!(m.contains("check what is already done on disk"), "{m}");
        assert!(resume_message("usage", TWO, MAIN).contains("Claude code 2 hit its usage limit"));
    }

    /// Bug note 2026-11-08T02-03-49: two Claude accounts offer the model and none was named; the one with quota runs it.
    #[test]
    fn a_new_task_with_no_account_named_goes_to_the_one_with_quota() {
        let reset = NOW + 86_400_000;
        let both = [MAIN, TWO];
        // Main spent by the reading (its weekly limit, 6 Nov 12:08 to 9 Nov 13:00): Claude code 2.
        assert_eq!(default_account(&both, &[acct(MAIN, Some(100.0), Some(reset)), acct(TWO, Some(3.0), Some(reset))], &[], NOW).as_deref(), Some(TWO));
        // Main spent by a limit Heimdall saw, with no reading at all: Claude code 2.
        assert_eq!(default_account(&both, &[], &[(MAIN.into(), reset)], NOW).as_deref(), Some(TWO));
        // That limit over: T3's order again, the main account.
        assert_eq!(default_account(&both, &[], &[(MAIN.into(), NOW - 1)], NOW).as_deref(), Some(MAIN));
        // Both with quota: the stable default, T3's first, whatever the use.
        assert_eq!(default_account(&both, &[acct(MAIN, Some(80.0), Some(reset)), acct(TWO, Some(1.0), Some(reset))], &[], NOW).as_deref(), Some(MAIN));
        // Both spent: still one (the task fails on its limit and the failover handles it), never an error.
        assert_eq!(default_account(&both, &[], &[(MAIN.into(), reset), (TWO.into(), reset)], NOW).as_deref(), Some(MAIN));
        // A Codex instance among the candidates: the caller says which.
        assert_eq!(default_account(&[MAIN, "codex"], &[], &[], NOW), None);
    }
}
