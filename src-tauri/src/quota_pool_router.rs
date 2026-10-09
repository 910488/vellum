//! Session-aware quota allocation. No prompts, credentials, or raw
//! conversation identities are retained here.
//!
//! Accounts differ in size, not only in how full they are: 10% left on a Pro
//! account outlasts 50% left on a Plus one. The usage endpoint reports only
//! percentages, so each account's size is a relative capacity — a prior from
//! its plan, replaced by a measurement once Vellum has routed enough tokens
//! through it to move its weekly percentage. New conversations go where the
//! active context load is smallest relative to the capacity still left.

use std::collections::HashMap;
use vellum_proxy_runtime::official_auth::OfficialRoutingContext;

const IDLE_MS: u64 = 30 * 60 * 1000;
const MAX_SESSIONS: usize = 1024;
const MAX_REQUESTS: usize = 4096;
/// A calibration sample needs the weekly percentage to have moved this much,
/// so integer rounding in the usage report stays a small error.
const CALIBRATION_MIN_POINTS: f64 = 2.0;
const CALIBRATION_MIN_TOKENS: f64 = 20_000.0;
/// Cached input is billed at a fraction of fresh input.
const CACHED_TOKEN_WEIGHT: f64 = 0.1;

pub(crate) struct Candidate {
    pub account_id: String,
    /// Spendable fraction left, 0–100, after floors and reserves.
    pub headroom: f64,
    /// Relative account size from the plan; Plus is 1.
    pub capacity_prior: f64,
    /// Raw weekly used percentage, the calibration signal.
    pub weekly_used: Option<f64>,
}

/// Relative weekly allowance by plan. Only a starting point: ChatGPT may
/// report both Pro tiers as `pro`, and workspace plans vary by seat, so
/// measured throughput replaces this as soon as there is one.
pub(crate) fn plan_capacity_prior(plan_type: Option<&str>) -> f64 {
    let plan = plan_type.map(|plan| plan.trim().to_ascii_lowercase());
    match plan.as_deref() {
        Some(plan) if plan.contains("lite") || plan.contains("5x") => 5.0,
        Some(plan) if plan.starts_with("pro") => 20.0,
        _ => 1.0,
    }
}

/// Tokens Vellum routed through one account since its weekly percentage was
/// last read, and the measured tokens per weekly point. Usage spent outside
/// Vellum on the same account reads as a smaller account, which errs toward
/// sending it less.
#[derive(Default)]
struct Calibration {
    anchor_used: Option<f64>,
    tokens_since: f64,
    tokens_per_point: Option<f64>,
}

impl Calibration {
    fn update(&mut self, used: f64) {
        match self.anchor_used {
            Some(anchor) if used + 0.5 < anchor => {
                // The weekly window reset; start over from the new baseline.
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            Some(anchor)
                if used - anchor >= CALIBRATION_MIN_POINTS
                    && self.tokens_since >= CALIBRATION_MIN_TOKENS =>
            {
                let sample = self.tokens_since / (used - anchor);
                self.tokens_per_point = Some(
                    self.tokens_per_point
                        .map_or(sample, |previous| 0.5 * previous + 0.5 * sample),
                );
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            Some(_) => {}
            None => self.anchor_used = Some(used),
        }
    }
}

struct Session {
    account_id: String,
    window: Option<String>,
    tokens: u64,
    cache_ratio: f64,
    touched: u64,
    latest_request: String,
}

struct RequestBinding {
    session: String,
    account_id: String,
    window: Option<String>,
    touched: u64,
}

#[derive(Default)]
pub(crate) struct SmartRouter {
    sessions: HashMap<String, Session>,
    requests: HashMap<String, RequestBinding>,
    /// Account size outlives sessions and settings changes.
    calibration: HashMap<String, Calibration>,
}

impl SmartRouter {
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.requests.clear();
    }

    fn prune(&mut self, now: u64) {
        self.sessions
            .retain(|_, session| now.saturating_sub(session.touched) < IDLE_MS);
        self.requests
            .retain(|_, request| now.saturating_sub(request.touched) < IDLE_MS);
    }

    /// Call only with eligible accounts, in user priority order. Admission
    /// gates are evaluated by the manager before reaching this method.
    pub fn choose(
        &mut self,
        candidates: &[Candidate],
        context: &OfficialRoutingContext,
        now: u64,
    ) -> (usize, &'static str) {
        assert!(!candidates.is_empty());
        self.prune(now);
        let Some(key) = context.session_key.as_ref() else {
            // Without a stable session identifier, priority is safer than
            // guessing which account owns an account-scoped continuation.
            return (0, "unidentified_session_rank");
        };
        let capacities = self.capacities(candidates);
        let previous = self.sessions.get(key);
        let incumbent = previous.and_then(|session| {
            candidates
                .iter()
                .position(|candidate| candidate.account_id == session.account_id)
        });
        let new_window = previous.is_some_and(|session|
            matches!((&session.window, &context.context_window_key), (Some(old), Some(new)) if old != new));
        let tokens = context.estimated_input_tokens.max(1);
        // An incremental continuation may contain only the delta. Preserve
        // the measured full context until an explicit context-window change.
        let tokens = if new_window {
            tokens
        } else {
            previous.map_or(tokens, |session| tokens.max(session.tokens))
        };
        let mut loads = vec![0_u64; candidates.len()];
        for (session_key, session) in &self.sessions {
            if session_key == key {
                continue;
            }
            if let Some(index) = candidates
                .iter()
                .position(|c| c.account_id == session.account_id)
            {
                loads[index] = loads[index].saturating_add(session.tokens);
            }
        }
        // Active context per unit of capacity still left. Equal-sized accounts
        // reduce to headroom and load; a 20x account takes twenty
        // conversations for every one a Plus account takes at equal fullness.
        let pressure = |index: usize| {
            loads[index].saturating_add(tokens) as f64
                / (candidates[index].headroom.max(0.01) * capacities[index])
        };
        let mut best = 0;
        for index in 1..candidates.len() {
            if pressure(index) < pressure(best) * (1.0 - 1e-9) {
                best = index;
            }
        }
        // How much less loaded `to` is than `from`, in points out of 100.
        let advantage = |from: usize, to: usize| 100.0 * (1.0 - pressure(to) / pressure(from));
        let (selected, reason) = match incumbent {
            // Preserve warm cache and provider continuation affinity. Account
            // changes mid-window happen only when the incumbent fails a gate.
            Some(index) if !new_window => (index, "session_affinity"),
            Some(index) => {
                let cache_ratio = previous.map_or(0.0, |session| session.cache_ratio);
                let pressure = context
                    .context_window
                    .filter(|window| *window > 0)
                    .map_or(0.0, |window| (tokens as f64 / window as f64).min(1.0));
                let switching_cost = 15.0 + 25.0 * cache_ratio + 25.0 * pressure;
                if advantage(index, best) > switching_cost {
                    (best, "context_window_rebalance")
                } else {
                    (index, "cache_affinity")
                }
            }
            None => (
                best,
                if previous.is_some() {
                    "session_account_unavailable"
                } else {
                    "balanced_new_session"
                },
            ),
        };
        let same_account =
            previous.is_some_and(|s| s.account_id == candidates[selected].account_id);
        let cache_ratio = if same_account {
            previous.map_or(0.0, |s| s.cache_ratio)
        } else {
            0.0
        };
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(key) {
            if let Some(oldest) = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone())
            {
                self.sessions.remove(&oldest);
            }
        }
        self.sessions.insert(
            key.clone(),
            Session {
                account_id: candidates[selected].account_id.clone(),
                window: context
                    .context_window_key
                    .clone()
                    .or_else(|| previous_window(self, key)),
                tokens,
                cache_ratio,
                touched: now,
                latest_request: context.request_id.clone(),
            },
        );
        if !context.request_id.is_empty() {
            if self.requests.len() >= MAX_REQUESTS {
                if let Some(oldest) = self
                    .requests
                    .iter()
                    .min_by_key(|(_, r)| r.touched)
                    .map(|(k, _)| k.clone())
                {
                    self.requests.remove(&oldest);
                }
            }
            self.requests.insert(
                context.request_id.clone(),
                RequestBinding {
                    session: key.clone(),
                    account_id: candidates[selected].account_id.clone(),
                    window: context.context_window_key.clone(),
                    touched: now,
                },
            );
        }
        (selected, reason)
    }

    /// Actual upstream input/cached tokens replace estimates. Failed or
    /// missing-usage turns cannot establish a cache hit. A late response from
    /// an old account must not overwrite a session that has since migrated.
    pub fn observe(&mut self, request_id: &str, input: u64, cached: u64, status: u16) {
        let Some(binding) = self.requests.remove(request_id) else {
            return;
        };
        if !(200..300).contains(&status) || input == 0 {
            return;
        }
        let billed = cached.min(input);
        self.calibration
            .entry(binding.account_id.clone())
            .or_default()
            .tokens_since += (input - billed) as f64 + CACHED_TOKEN_WEIGHT * billed as f64;
        if let Some(session) = self.sessions.get_mut(&binding.session) {
            if session.account_id == binding.account_id
                && session.latest_request == request_id
                && binding
                    .window
                    .as_ref()
                    .is_none_or(|window| session.window.as_ref() == Some(window))
            {
                session.tokens = input;
                session.cache_ratio = cached.min(input) as f64 / input as f64;
            }
        }
    }
}

impl SmartRouter {
    /// Measured sizes where they exist, put in plan units through the
    /// measured accounts so measured and unmeasured accounts still compare.
    fn capacities(&mut self, candidates: &[Candidate]) -> Vec<f64> {
        for candidate in candidates {
            if let Some(used) = candidate.weekly_used {
                self.calibration
                    .entry(candidate.account_id.clone())
                    .or_default()
                    .update(used);
            }
        }
        let measured = |candidate: &Candidate| {
            self.calibration
                .get(&candidate.account_id)
                .and_then(|calibration| calibration.tokens_per_point)
        };
        let mut per_unit = candidates
            .iter()
            .filter_map(|candidate| {
                measured(candidate).map(|tokens| tokens / candidate.capacity_prior.max(0.01))
            })
            .collect::<Vec<_>>();
        per_unit.sort_by(f64::total_cmp);
        let unit = per_unit.get(per_unit.len() / 2).copied();
        candidates
            .iter()
            .map(|candidate| match (measured(candidate), unit) {
                (Some(tokens), Some(unit)) if unit > 0.0 => (tokens / unit).clamp(0.05, 100.0),
                _ => candidate.capacity_prior.max(0.01),
            })
            .collect()
    }
}

fn previous_window(router: &SmartRouter, key: &str) -> Option<String> {
    router.sessions.get(key).and_then(|s| s.window.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(session: &str, tokens: u64, window: &str) -> OfficialRoutingContext {
        OfficialRoutingContext {
            session_key: Some(session.into()),
            context_window_key: Some(window.into()),
            request_id: format!("{session}-{window}"),
            estimated_input_tokens: tokens,
            context_window: Some(100_000),
        }
    }
    fn candidate(id: &str, headroom: f64, capacity_prior: f64) -> Candidate {
        Candidate {
            account_id: id.into(),
            headroom,
            capacity_prior,
            weekly_used: None,
        }
    }
    fn accounts(a: f64, b: f64) -> Vec<Candidate> {
        vec![candidate("a", a, 1.0), candidate("b", b, 1.0)]
    }
    #[test]
    fn new_sessions_balance_context_load_with_priority_ties() {
        let mut router = SmartRouter::default();
        let candidates = accounts(80.0, 80.0);
        assert_eq!(
            router
                .choose(&candidates, &context("large", 90_000, "w1"), 1)
                .0,
            0
        );
        assert_eq!(
            router
                .choose(&candidates, &context("small", 1_000, "w1"), 2)
                .0,
            1
        );
        assert_eq!(
            router
                .choose(&candidates, &context("third", 1_000, "w1"), 3)
                .0,
            1
        );
    }
    #[test]
    fn keeps_warm_session_even_when_other_account_has_more_quota() {
        let mut router = SmartRouter::default();
        let request = context("s", 50_000, "w1");
        router.choose(&accounts(80.0, 70.0), &request, 1);
        router.observe(&request.request_id, 60_000, 55_000, 200);
        assert_eq!(
            router.choose(&accounts(10.0, 90.0), &context("s", 50, "w1"), 2),
            (0, "session_affinity")
        );
        assert_eq!(router.sessions["s"].tokens, 60_000);
    }
    #[test]
    fn rebalances_at_compaction_but_honors_cache_and_context_switch_cost() {
        let mut router = SmartRouter::default();
        let request = context("s", 90_000, "w1");
        router.choose(&accounts(80.0, 70.0), &request, 1);
        router.observe(&request.request_id, 90_000, 80_000, 200);
        assert_eq!(
            router
                .choose(&accounts(40.0, 70.0), &context("s", 90_000, "w2"), 2)
                .0,
            0
        );
        assert_eq!(
            router.choose(&accounts(10.0, 90.0), &context("s", 1_000, "w3"), 3),
            (1, "context_window_rebalance")
        );
    }
    #[test]
    fn unavailable_incumbent_migrates_and_late_usage_cannot_restore_it() {
        let mut router = SmartRouter::default();
        let request = context("s", 5_000, "w1");
        router.choose(&accounts(80.0, 70.0), &request, 1);
        let mut second = request.clone();
        second.request_id = "second".into();
        let only_b = vec![candidate("b", 70.0, 1.0)];
        assert_eq!(
            router.choose(&only_b, &second, 2).1,
            "session_account_unavailable"
        );
        router.observe(&request.request_id, 80_000, 70_000, 200);
        assert_eq!(router.sessions["s"].tokens, 5_000);
    }
    #[test]
    fn idle_sessions_expire_and_unknown_identity_uses_rank() {
        let mut router = SmartRouter::default();
        router.choose(&accounts(80.0, 70.0), &context("s", 50_000, "w1"), 1);
        assert_eq!(
            router
                .choose(
                    &accounts(10.0, 90.0),
                    &context("s", 50_000, "w1"),
                    IDLE_MS + 1
                )
                .0,
            1
        );
        assert_eq!(
            router.choose(
                &accounts(10.0, 90.0),
                &OfficialRoutingContext::default(),
                IDLE_MS + 2
            ),
            (0, "unidentified_session_rank")
        );
    }
    #[test]
    fn plan_priors_map_plus_and_pro_tiers() {
        assert_eq!(plan_capacity_prior(Some("plus")), 1.0);
        assert_eq!(plan_capacity_prior(Some("Pro")), 20.0);
        assert_eq!(plan_capacity_prior(Some("prolite")), 5.0);
        assert_eq!(plan_capacity_prior(Some("team")), 1.0);
        assert_eq!(plan_capacity_prior(None), 1.0);
    }
    #[test]
    fn equally_full_accounts_share_new_conversations_by_size() {
        let mut router = SmartRouter::default();
        let candidates = vec![candidate("plus", 80.0, 1.0), candidate("pro", 80.0, 20.0)];
        let mut on_plus = 0;
        for session in 0..210 {
            let picked = router
                .choose(&candidates, &context(&format!("s{session}"), 1_000, "w1"), 1)
                .0;
            on_plus += usize::from(picked == 0);
        }
        assert!((9..=11).contains(&on_plus), "plus took {on_plus} of 210");
    }
    #[test]
    fn a_nearly_empty_large_account_still_outlasts_a_fresh_small_one() {
        let mut router = SmartRouter::default();
        let candidates = vec![candidate("plus", 90.0, 1.0), candidate("pro", 10.0, 20.0)];
        assert_eq!(router.choose(&candidates, &context("s", 1_000, "w1"), 1).0, 1);
    }
    #[test]
    fn measured_throughput_replaces_the_plan_prior() {
        let mut router = SmartRouter::default();
        // Both report the same plan; "big" turns out to move 4x slower.
        let at = |small: f64, big: f64| {
            vec![
                Candidate {
                    weekly_used: Some(small),
                    ..candidate("small", 50.0, 20.0)
                },
                Candidate {
                    weekly_used: Some(big),
                    ..candidate("big", 50.0, 20.0)
                },
            ]
        };
        router.capacities(&at(10.0, 10.0));
        for account in ["small", "big"] {
            router.requests.insert(
                account.into(),
                RequestBinding {
                    session: account.into(),
                    account_id: account.into(),
                    window: None,
                    touched: 1,
                },
            );
            router.observe(account, 400_000, 0, 200);
        }
        let capacities = router.capacities(&at(18.0, 12.0));
        assert!(
            (capacities[1] / capacities[0] - 4.0).abs() < 1e-6,
            "{capacities:?}"
        );
        // A weekly reset moves the baseline instead of producing a sample.
        assert_eq!(router.capacities(&at(1.0, 1.0)), capacities);
    }
}
