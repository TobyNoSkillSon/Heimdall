//! Compaction off the user's path (night pass 19→20 Oct, H1; lab/harness/EXPECTED.md E5.2, E5.3, E5.6).
//!
//! 19 Oct 18:12 the first wake after the agent came on waited 45 s: the cold-cache compaction ran inside that wake's
//! prompt (the Pi extension compacted on `input`). The user: compaction happens while idle (at agent start or after a
//! turn), never in front of his request. So every compaction Heimdall starts on its own runs while the agent is idle
//! and the user has been quiet for a while. Once started it is never cancelled ("When the agent is compacting I
//! should not be able to interrupt you … which is just wasting money"): his words, notices and clicks queue behind it
//! and go out as it ends, and his Stop waits for it too.
//!
//! - Threshold: once the history fills 85% of the request budget (`Limits::compact_at`). Pi's own
//!   threshold, which compacts inside a turn, sits higher, at the budget less the reserve and 90% at most (Opus 5.5
//!   with its 252,571-token budget: idle mark 214,685, Pi 227,313, so 12,628 tokens of room; the 19 Oct session grew
//!   at most 11,000 tokens between two requests).
//! - A model switch to a smaller budget (audit 27 Oct, F6): a history past the new budget that still fits the model's
//!   real window is carried (`Limits::carrying`): Pi's in-turn check moves up for that start, so the summary runs here,
//!   at the first quiet moment, not in front of the user's next request.
//! - Cold cache: the provider's one-hour prompt cache for the conversation has expired (or the last request ran on
//!   another model) and the history is past the break-even size (`harness::cold_cache_break_even`): summarising
//!   then costs less than re-sending it as a new cache write.
//! - Quiet: no wake, typed message or turn for `quiet_ms` (30 s, the follow-up window; a start counts as activity, so
//!   speech right after Agent on goes out at once).
//!
//! Keeping the cache warm: on a provider with the one-hour cache, once 55 minutes pass without a
//! request Heimdall sends one short cache-refresh message (`KeepWarm`). Its request reads the cached history, which
//! refreshes the hour for free, and the agent answers with a quick stall check, else `NO_REPLY` (silent). After 3
//! hours without a real request the pings stop and the cache goes cold (the cold-cache rule above takes over).
use crate::provider::Limits;

/// No compaction starts within this long after the last wake, typed message, turn or start.
pub const QUIET_MS: u64 = 30_000;
/// Anthropic's long prompt-cache lifetime.
pub const CACHE_TTL_MS: u64 = 60 * 60_000;

/// When the provider's cache for this conversation was last written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastRequest {
    /// Wall-clock start of the last request that carried the history.
    pub at_ms: u64,
    /// It ran on the model that runs now (another model's cache does not serve this one).
    pub same_model: bool,
}

/// The cold-cache rule: compact once the cache has expired and the history is past the break-even size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColdCache {
    pub ttl_ms: u64,
    pub min_tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Compact while idle once the history has at least this many tokens.
    pub mark: u64,
    pub cold: Option<ColdCache>,
    /// Pi's own threshold this start runs with (`Limits::pi_threshold`): past it Pi compacts in front of a request, so
    /// the start context (E4.2) compacts first only past it.
    pub in_turn: u64,
    /// This start carries a history from a bigger budget (`Limits::carrying`): its first successful compaction ends
    /// that, and the host reloads Pi onto the budget's own threshold.
    pub carrying: bool,
    /// Quiet needed after the last activity before a compaction starts (`QUIET_MS`).
    pub quiet_ms: u64,
    /// The cache-refresh ping, on providers whose hour cache a request keeps alive.
    pub warm: Option<KeepWarm>,
}

/// A cache-refresh ping goes out once the last request is this old (5 minutes inside the hour).
pub const PING_AFTER_MS: u64 = 55 * 60_000;
/// No ping once the last real request (anything but a ping) is this old.
pub const PING_STOP_MS: u64 = 3 * 60 * 60_000;
/// What the agent reads in a ping. The instructions ride the message so the system prompt (and its cache) stay as
/// they are.
pub const PING_TEXT: &str = "[Heimdall cache refresh]\nNo request: this message keeps the prompt cache warm. Check \
quickly whether anything stalled (a task or worker with no progress, a notice or question you have not acted on). Act \
only if something needs it. Otherwise answer only NO_REPLY.";

/// The ping after a start, before the user has talked: the agent does nothing on its own then (the restart message).
pub const PING_TEXT_AFTER_START: &str = "[Heimdall cache refresh]\nNo request: this message keeps the prompt cache warm. \
User has not talked since the restart, so answer only NO_REPLY.";

/// When to send a cache-refresh ping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepWarm {
    pub after_ms: u64,
    pub stop_ms: u64,
    pub ttl_ms: u64,
}
impl KeepWarm {
    pub const HOUR_CACHE: Self = Self { after_ms: PING_AFTER_MS, stop_ms: PING_STOP_MS, ttl_ms: CACHE_TTL_MS };
    /// A ping is due at `now_ms`: the agent is on, the cache for this model is still alive but `after_ms` old, and the
    /// last real request (`last_real_ms`, which pings never move) is under `stop_ms` old. A cache that already expired
    /// is not pinged: a ping would only write it again at full price.
    pub fn due(&self, agent_on: bool, last: Option<LastRequest>, last_real_ms: u64, now_ms: u64) -> bool {
        let Some(last) = last.filter(|l| l.same_model) else { return false };
        let idle = now_ms.saturating_sub(last.at_ms);
        agent_on && idle >= self.after_ms && idle < self.ttl_ms && now_ms.saturating_sub(last_real_ms) < self.stop_ms
    }
}
impl CompactionPolicy {
    /// The policy for a request budget; `hour_cache` when the provider keeps a one-hour prompt cache.
    pub fn for_limits(limits: Limits, hour_cache: bool) -> Self {
        Self {
            mark: limits.compact_at(),
            in_turn: limits.pi_threshold(),
            carrying: limits.carry > 0,
            cold: hour_cache.then(|| ColdCache { ttl_ms: CACHE_TTL_MS, min_tokens: crate::harness::cold_cache_break_even(limits) }),
            quiet_ms: QUIET_MS,
            warm: hour_cache.then_some(KeepWarm::HOUR_CACHE),
        }
    }
    /// The cache has gone cold at `now_ms`: the last request is older than the cache lifetime or ran on another
    /// model. Nothing cached (no request since the last compaction) is never cold.
    pub fn cache_cold(&self, last: Option<LastRequest>, now_ms: u64) -> bool {
        match (self.cold, last) {
            (Some(cold), Some(last)) => !last.same_model || now_ms >= last.at_ms.saturating_add(cold.ttl_ms),
            _ => false,
        }
    }
    /// Why the history should be compacted now, if it should: `tokens` is Pi's estimate of the next request.
    pub fn reason(&self, tokens: u64, quiet: bool, cold: bool) -> Option<&'static str> {
        if !quiet {
            return None;
        }
        if tokens >= self.mark {
            Some("threshold")
        } else if cold && self.cold.is_some_and(|c| tokens >= c.min_tokens) {
            Some("cold_cache")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn opus() -> Limits {
        Limits::for_context(252_571, Some(128_000))
    }
    #[test]
    fn heimdall_compacts_at_85_percent_before_pis_threshold() {
        let limits = opus();
        let policy = CompactionPolicy::for_limits(limits, true);
        // Compact at about 85% of the budget (it fired at 190,617 tokens, 75%, on 25 Oct).
        assert_eq!(policy.mark, 214_685);
        assert_eq!(policy.mark, limits.context * 85 / 100);
        // Pi's in-turn check at 90% at most (audit 27 Oct, F5), the reserve's 16,384 below the budget otherwise.
        assert_eq!(limits.threshold(), 252_571 * 90 / 100);
        assert_eq!(policy.in_turn, limits.threshold());
        // 19 Oct: at most 11,000 tokens between two requests, so Pi's in-turn check is not reached between idle moments.
        assert!(limits.threshold() - policy.mark > 11_000);
        assert_eq!(policy.reason(214_684, true, false), None);
        assert_eq!(policy.reason(214_685, true, false), Some("threshold"));
        // The default 128K budget and a small local window: 85%, never past Pi's own threshold.
        let l = Limits::for_context(128_000, Some(128_000));
        assert_eq!((CompactionPolicy::for_limits(l, false).mark, l.threshold()), (108_800, 112_000));
        let tiny = Limits::for_context(8_192, None);
        assert_eq!(CompactionPolicy::for_limits(tiny, false).mark, tiny.threshold());
        assert_eq!(policy.cold, Some(ColdCache { ttl_ms: 3_600_000, min_tokens: 92_000 }));
        // A local engine has no hour cache.
        assert_eq!(CompactionPolicy::for_limits(Limits::for_context(32_768, None), false).cold, None);
        assert_eq!(CompactionPolicy::for_limits(Limits::for_context(32_768, None), false).warm, None, "no hour cache, no ping");
        assert_eq!(policy.warm, Some(KeepWarm::HOUR_CACHE));
    }
    #[test]
    fn a_ping_goes_after_55_idle_minutes_stops_after_3_hours_and_never_with_the_agent_off() {
        let warm = KeepWarm::HOUR_CACHE;
        const MIN: u64 = 60_000;
        let at = 1_000_000;
        let last = |t| Some(LastRequest { at_ms: t, same_model: true });
        assert!(!warm.due(true, last(at), at, at + 54 * MIN), "not before 55 minutes");
        assert!(warm.due(true, last(at), at, at + 55 * MIN));
        assert!(!warm.due(false, last(at), at, at + 55 * MIN), "never with the Agent off");
        assert!(!warm.due(true, last(at), at, at + 60 * MIN), "an expired cache is not pinged");
        assert!(!warm.due(true, None, at, at + 55 * MIN), "nothing cached");
        assert!(!warm.due(true, Some(LastRequest { at_ms: at, same_model: false }), at, at + 55 * MIN), "another model's cache");
        // Each ping's request is the next last request: pings at 55, 110 and 165 minutes, none at 220 (3 h after the
        // last real request).
        let (mut req, mut pings) = (at, vec![]);
        for m in 1..=300 {
            let now = at + m * MIN;
            if warm.due(true, last(req), at, now) { pings.push(m); req = now; }
        }
        assert_eq!(pings, [55, 110, 165]);
    }
    #[test]
    fn cold_only_after_the_cache_lifetime_or_on_another_model() {
        let policy = CompactionPolicy::for_limits(opus(), true);
        let at = 1_000_000;
        let last = Some(LastRequest { at_ms: at, same_model: true });
        assert!(!policy.cache_cold(last, at + CACHE_TTL_MS - 1));
        assert!(policy.cache_cold(last, at + CACHE_TTL_MS));
        assert!(policy.cache_cold(Some(LastRequest { at_ms: at, same_model: false }), at + 1));
        assert!(!policy.cache_cold(None, at + 10 * CACHE_TTL_MS), "nothing cached since the last compaction");
        assert!(!CompactionPolicy::for_limits(opus(), false).cache_cold(last, at + 10 * CACHE_TTL_MS));
    }
    #[test]
    fn reasons_need_quiet_and_size() {
        let policy = CompactionPolicy::for_limits(opus(), true);
        assert_eq!(policy.reason(policy.mark, false, true), None, "never while User is active");
        assert_eq!(policy.reason(policy.mark, true, false), Some("threshold"));
        assert_eq!(policy.reason(98_000, true, true), Some("cold_cache"));
        assert_eq!(policy.reason(98_000, true, false), None);
        assert_eq!(policy.reason(91_999, true, true), None, "below break-even re-sending is cheaper");
    }
}
