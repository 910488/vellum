//! Fair consumption rates with account-scoped cache affinity.
//! Only hashes, counts and quota observations are retained; never prompts.
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use vellum_proxy_runtime::official_auth::OfficialRoutingContext;

const IDLE_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_SESSIONS: usize = 1024;
const MAX_REQUESTS: usize = 4096;
const HALF_LIFE_SECONDS: f64 = 1200.0;
const DECAY: f64 = std::f64::consts::LN_2 / HALF_LIFE_SECONDS;
const MIGRATION_INTERVAL_MS: u64 = 30 * 60 * 1000;
const CACHED_TOKEN_WEIGHT: f64 = 0.1;
/// Bootstrap Plus allowance: 1M effective input tokens/week. Measurements
/// replace this prior; unknown resets conservatively assume a full window.
const PRIOR_TOKENS_PER_POINT: f64 = 10_000.0;
pub(crate) const DEFAULT_LAMBDA: f64 = 0.1;

#[derive(Clone, Default)]
pub(crate) struct Candidate {
    pub account_id: String,
    pub headroom: f64,
    pub capacity_prior: f64,
    pub weekly_used: Option<f64>,
    /// Raw spendable percentage points, not renormalized after the floor.
    pub weekly_headroom: Option<f64>,
    pub weekly_reset_at: Option<u64>,
    pub five_hour_used: Option<f64>,
    pub five_hour_reset_at: Option<u64>,
}

pub(crate) fn plan_capacity_prior(plan_type: Option<&str>) -> f64 {
    let plan = plan_type.map(|plan| plan.trim().to_ascii_lowercase());
    match plan.as_deref() {
        Some(plan) if plan.contains("lite") || plan.contains("5x") => 5.0,
        Some(plan) if plan.starts_with("pro") => 20.0,
        _ => 1.0,
    }
}

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
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            Some(anchor) if used - anchor >= 2.0 && self.tokens_since >= 20_000.0 => {
                let sample = self.tokens_since / (used - anchor);
                self.tokens_per_point = Some(
                    self.tokens_per_point
                        .map_or(sample, |p| 0.5 * p + 0.5 * sample),
                );
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            None => {
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Rate {
    value: f64,
    at: u64,
}
impl Rate {
    fn at(self, now: u64) -> f64 {
        self.value * (-DECAY * now.saturating_sub(self.at) as f64 / 1000.0).exp()
    }
    fn add(&mut self, value: f64, now: u64) {
        if now < self.at {
            self.value =
                (self.value + value * (-DECAY * (self.at - now) as f64 / 1000.0).exp()).max(0.0);
        } else {
            self.value = (self.at(now) + value).max(0.0);
            self.at = now;
        }
    }
}
struct Session {
    account_id: String,
    window: Option<String>,
    tokens: u64,
    rate: Rate,
    measured: Rate,
    started: u64,
    touched: u64,
    migrated: Option<u64>,
    review: bool,
    observations: u32,
    latest_request: String,
}
struct RequestBinding {
    session: String,
    account_id: String,
    touched: u64,
}

/// Cache survival estimates from the local 7-day Official replay (2026-10-12):
/// <=10m median reuse .97–1; 10–30m .40–.66; >1h median zero.
pub(crate) fn cache_heat(gap_ms: u64) -> f64 {
    match gap_ms {
        0..=600_000 => 0.95,
        600_001..=1_800_000 => 0.5,
        1_800_001..=3_600_000 => 0.3,
        _ => 0.05,
    }
}

pub(crate) struct SmartRouter {
    sessions: HashMap<String, Session>,
    requests: HashMap<String, RequestBinding>,
    calibration: HashMap<String, Calibration>,
    five_calibration: HashMap<String, Calibration>,
    loads: HashMap<String, Rate>,
    samples: [VecDeque<(String, f64)>; 2],
    lambda: f64,
    placement_only: bool,
}
impl Default for SmartRouter {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            requests: HashMap::new(),
            calibration: HashMap::new(),
            five_calibration: HashMap::new(),
            loads: HashMap::new(),
            samples: Default::default(),
            lambda: DEFAULT_LAMBDA,
            placement_only: false,
        }
    }
}
impl SmartRouter {
    /// Offline comparison uses the very same production decision path.
    #[allow(dead_code)] // Used by the offline example, not the desktop binary.
    pub fn for_replay(lambda: f64, placement_only: bool) -> Self {
        Self {
            lambda,
            placement_only,
            ..Self::default()
        }
    }
    pub fn clear(&mut self) {
        self.sessions.clear();
        self.requests.clear();
        self.loads.clear();
    }
    fn prior(&self, review: bool) -> f64 {
        let mut values = self.samples[usize::from(review)]
            .iter()
            .map(|(_, rate)| *rate)
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        // Cold start only; learned main/review medians never mix.
        values
            .get(values.len() / 2)
            .copied()
            .unwrap_or(if review { 5.0 } else { 25.0 })
            .max(0.01)
    }
    fn fair_rates(&mut self, candidates: &[Candidate], now: u64) -> Vec<(f64, f64, bool)> {
        candidates
            .iter()
            .map(|c| {
                let cal = self.calibration.entry(c.account_id.clone()).or_default();
                if let Some(used) = c.weekly_used {
                    cal.update(used);
                }
                let capacity = cal
                    .tokens_per_point
                    .unwrap_or(PRIOR_TOKENS_PER_POINT * c.capacity_prior.max(0.01));
                let remaining = c.weekly_headroom.unwrap_or(c.headroom).max(0.0);
                let until = c
                    .weekly_reset_at
                    .map_or(7.0 * 86400.0, |t| t.saturating_sub(now) as f64 / 1000.0)
                    .max(60.0);
                let mut fair = remaining * capacity / until;
                if let Some(used) = c.five_hour_used {
                    let five = self
                        .five_calibration
                        .entry(c.account_id.clone())
                        .or_default();
                    five.update(used);
                    let left = (95.0 - used).max(0.0);
                    if let Some(size) = five.tokens_per_point {
                        let until = c
                            .five_hour_reset_at
                            .map_or(18000.0, |t| t.saturating_sub(now) as f64 / 1000.0)
                            .max(60.0);
                        fair = fair.min(left * size / until);
                    } else {
                        // No measured 5h capacity: retain hard admission gate and
                        // gradually penalize the last 15 spendable points.
                        fair *= (left / 15.0).clamp(0.01, 1.0);
                    }
                }
                (
                    fair.max(0.000_001),
                    capacity,
                    cal.tokens_per_point.is_some(),
                )
            })
            .collect()
    }
    pub fn choose(
        &mut self,
        candidates: &[Candidate],
        context: &OfficialRoutingContext,
        now: u64,
    ) -> (usize, &'static str) {
        assert!(!candidates.is_empty());
        self.sessions
            .retain(|_, s| now.saturating_sub(s.touched) < IDLE_MS);
        self.requests
            .retain(|_, r| now.saturating_sub(r.touched) < IDLE_MS);
        let fair = self.fair_rates(candidates, now);
        let loads = candidates
            .iter()
            .map(|c| {
                self.loads
                    .get(&c.account_id)
                    .copied()
                    .unwrap_or_default()
                    .at(now)
            })
            .collect::<Vec<_>>();
        let Some(key) = context.session_key.as_ref() else {
            self.log_decision(
                candidates,
                &fair,
                &loads,
                0,
                "unidentified_session_rank",
                (0.0, 0, 0, 0.0, 0.0),
            );
            return (0, "unidentified_session_rank");
        };
        let previous = self.sessions.get(key);
        let incumbent =
            previous.and_then(|s| candidates.iter().position(|c| c.account_id == s.account_id));
        let review = key.starts_with("review:");
        let rho = previous.map_or_else(|| self.prior(review), |s| s.rate.at(now).max(0.01));
        let gap = previous.map_or(0, |s| now.saturating_sub(s.touched));
        let compressed = previous.is_some_and(
            |s| matches!((&s.window,&context.context_window_key),(Some(a),Some(b)) if a!=b),
        );
        let tokens = previous.map_or(context.estimated_input_tokens, |s| s.tokens);
        let cost = if compressed {
            0.0
        } else {
            0.9 * tokens as f64 * cache_heat(gap)
        };
        let mut best = incumbent.unwrap_or(0);
        let projected =
            |i: usize| (loads[i] + if Some(i) == incumbent { 0.0 } else { rho }) / fair[i].0;
        for i in 0..candidates.len() {
            if projected(i) < projected(best) * (1.0 - 1e-9) {
                best = i;
            }
        }
        let mut benefit = 0.0;
        let (selected, reason) = match incumbent {
            None => (
                best,
                if previous.is_some() {
                    "gate"
                } else {
                    "new_session_min_util"
                },
            ),
            Some(i) => {
                let age = previous.map_or(0, |s| now.saturating_sub(s.started));
                let horizon = (age as f64 / 1000.0).min(3600.0);
                let over = |load: f64, rate: f64| (load - rate).max(0.0);
                benefit = self.lambda
                    * horizon
                    * (over(loads[i], fair[i].0) + over(loads[best], fair[best].0)
                        - over((loads[i] - rho).max(0.0), fair[i].0)
                        - over(loads[best] + rho, fair[best].0));
                if best == i {
                    benefit = 0.0;
                }
                let cooled = compressed || gap > 600_000;
                let weekly_left = candidates[i]
                    .weekly_headroom
                    .unwrap_or(candidates[i].headroom)
                    * fair[i].1;
                let five_left = candidates[i].five_hour_used.and_then(|used| {
                    self.five_calibration
                        .get(&candidates[i].account_id)
                        .and_then(|c| c.tokens_per_point)
                        .map(|size| (95.0 - used).max(0.0) * size)
                });
                let near = weekly_left.min(five_left.unwrap_or(f64::INFINITY)) <= tokens as f64
                    || (five_left.is_none()
                        && candidates[i]
                            .five_hour_used
                            .is_some_and(|used| used >= 90.0));
                let hysteresis = previous.is_none_or(|s| {
                    s.migrated
                        .is_none_or(|t| now.saturating_sub(t) >= MIGRATION_INTERVAL_MS)
                });
                if !self.placement_only && best != i && hysteresis && cooled && near {
                    (best, "migrate_near_limit")
                } else if !self.placement_only && best != i && hysteresis && benefit > cost * 1.25 {
                    (
                        best,
                        if cooled {
                            "migrate_cold_cache"
                        } else {
                            "migrate_benefit"
                        },
                    )
                } else {
                    (i, "stay_cache_hot")
                }
            }
        };
        self.log_decision(
            candidates,
            &fair,
            &loads,
            selected,
            reason,
            (rho, tokens, gap, cost, benefit),
        );
        let previous = self.sessions.remove(key);
        let changed = previous
            .as_ref()
            .is_some_and(|s| s.account_id != candidates[selected].account_id);
        if let Some(s) = &previous {
            if changed {
                self.loads
                    .entry(s.account_id.clone())
                    .or_default()
                    .add(-rho, now);
            }
        }
        if previous.is_none() || changed {
            self.loads
                .entry(candidates[selected].account_id.clone())
                .or_default()
                .add(rho, now);
        }
        if self.sessions.len() >= MAX_SESSIONS {
            if let Some(old) = self
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.touched)
                .map(|(k, _)| k.clone())
            {
                self.sessions.remove(&old);
            }
        }
        let mut session = previous.unwrap_or_else(|| Session {
            account_id: candidates[selected].account_id.clone(),
            window: None,
            tokens: context.estimated_input_tokens,
            rate: Rate {
                value: rho,
                at: now,
            },
            measured: Rate::default(),
            started: now,
            touched: now,
            migrated: None,
            review,
            observations: 0,
            latest_request: String::new(),
        });
        session.account_id = candidates[selected].account_id.clone();
        if changed {
            session.migrated = Some(now);
        }
        if compressed {
            session.tokens = context.estimated_input_tokens;
        }
        session.window = context.context_window_key.clone().or(session.window);
        session.touched = now;
        session.latest_request = context.request_id.clone();
        self.sessions.insert(key.clone(), session);
        if !context.request_id.is_empty() {
            if self.requests.len() >= MAX_REQUESTS {
                if let Some(old) = self
                    .requests
                    .iter()
                    .min_by_key(|(_, r)| r.touched)
                    .map(|(k, _)| k.clone())
                {
                    self.requests.remove(&old);
                }
            }
            self.requests.insert(
                context.request_id.clone(),
                RequestBinding {
                    session: key.clone(),
                    account_id: candidates[selected].account_id.clone(),
                    touched: now,
                },
            );
        }
        (selected, reason)
    }
    fn log_decision(
        &self,
        candidates: &[Candidate],
        fair: &[(f64, f64, bool)],
        loads: &[f64],
        selected: usize,
        reason: &str,
        session: (f64, u64, u64, f64, f64),
    ) {
        let selected_hash = format!(
            "{:x}",
            Sha256::digest(candidates[selected].account_id.as_bytes())
        );
        let candidates = candidates.iter().enumerate().map(|(i,c)| {
            let id=format!("{:x}",Sha256::digest(c.account_id.as_bytes()));
            format!("account={} headroom={:.3} weekly_used={:?} capacity={:.3} source={} L={:.3} f={:.3} u={:.3}",
                &id[..12],c.weekly_headroom.unwrap_or(c.headroom),c.weekly_used,fair[i].1,
                if fair[i].2 {"measured"} else {"prior"},loads[i],fair[i].0,loads[i]/fair[i].0)
        }).collect::<Vec<_>>().join("; ");
        log::info!("[QuotaPool] strategy=balanced candidates=[{}] rho={:.3} X={} gap_ms={} cost={:.3} benefit={:.3} selected_rank={} selected_account={} reason={}",
            candidates,session.0,session.1,session.2,session.3,session.4,selected+1,&selected_hash[..12],reason);
    }
    pub fn observe(&mut self, request_id: &str, input: u64, cached: u64, status: u16) {
        let Some(binding) = self.requests.remove(request_id) else {
            return;
        };
        if !(200..300).contains(&status) || input == 0 {
            return;
        }
        let cached = cached.min(input);
        let billed = (input - cached) as f64 + CACHED_TOKEN_WEIGHT * cached as f64;
        self.calibration
            .entry(binding.account_id.clone())
            .or_default()
            .tokens_since += billed;
        self.five_calibration
            .entry(binding.account_id.clone())
            .or_default()
            .tokens_since += billed;
        self.loads
            .entry(binding.account_id.clone())
            .or_default()
            .add(billed * DECAY, binding.touched);
        if let Some(s) = self.sessions.get_mut(&binding.session) {
            // Older responses still count toward the actual account load, but
            // must not mutate a newer request's context or migrated forecast.
            if s.account_id == binding.account_id && s.latest_request == request_id {
                s.tokens = input;
                s.rate.add(billed * DECAY, binding.touched);
                s.measured.add(billed * DECAY, binding.touched);
                s.observations += 1;
                if s.observations >= 2 {
                    let list = &mut self.samples[usize::from(s.review)];
                    // One sample per session: a fast session must not dominate
                    // the median simply by completing more requests.
                    if let Some(index) = list.iter().position(|(key, _)| key == &binding.session) {
                        list.remove(index);
                    }
                    if list.len() == 128 {
                        list.pop_front();
                    }
                    list.push_back((
                        binding.session.clone(),
                        s.measured.at(binding.touched).max(0.01),
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context(session: &str, at: u64, input: u64) -> OfficialRoutingContext {
        OfficialRoutingContext {
            session_key: Some(session.into()),
            request_id: format!("{session}-{at}"),
            estimated_input_tokens: input,
            context_window_key: Some("w1".into()),
            ..Default::default()
        }
    }
    fn candidate(id: &str, headroom: f64) -> Candidate {
        Candidate {
            account_id: id.into(),
            headroom,
            capacity_prior: 1.0,
            ..Default::default()
        }
    }
    #[test]
    fn concurrent_new_sessions_reserve_load_before_usage_arrives() {
        let mut router = SmartRouter::default();
        let accounts = vec![
            candidate("a", 80.0),
            candidate("b", 80.0),
            candidate("c", 80.0),
        ];
        let choices = (0..3)
            .map(|n| {
                router
                    .choose(&accounts, &context(&format!("s{n}"), 1, 100), 1)
                    .0
            })
            .collect::<Vec<_>>();
        assert_eq!(choices, vec![0, 1, 2]);
    }
    #[test]
    fn new_session_avoids_high_actual_consumption_rate() {
        let mut router = SmartRouter::default();
        let accounts = vec![candidate("a", 80.0), candidate("b", 80.0)];
        for n in 0..20 {
            let now = 1 + n * 60_000;
            let request = context("long", now, 1000);
            assert_eq!(router.choose(&accounts, &request, now).0, 0);
            router.observe(&request.request_id, 200_000, 0, 200);
        }
        assert_eq!(
            router
                .choose(&accounts, &context("new", 1_200_001, 100), 1_200_001)
                .0,
            1
        );
    }
    #[test]
    fn actual_cached_cost_and_main_review_priors_are_separate() {
        let mut router = SmartRouter::default();
        let accounts = vec![candidate("a", 80.0)];
        for now in [1, 60_001] {
            let req = context("review:r", now, 1000);
            router.choose(&accounts, &req, now);
            router.observe(&req.request_id, 100_000, 100_000, 200);
        }
        assert_eq!(router.calibration["a"].tokens_since, 20_000.0);
        assert_eq!(router.prior(false), 25.0);
        assert!(router.prior(true) < 25.0);
    }
    #[test]
    fn reset_time_and_measured_five_hour_capacity_limit_fair_rate() {
        let mut router = SmartRouter::default();
        let now = 1_000_000;
        let mut a = candidate("a", 80.0);
        a.weekly_reset_at = Some(now + 3_600_000);
        a.five_hour_used = Some(90.0);
        a.five_hour_reset_at = Some(now + 1_800_000);
        router.five_calibration.insert(
            "a".into(),
            Calibration {
                tokens_per_point: Some(1000.0),
                ..Default::default()
            },
        );
        let rates = router.fair_rates(&[a], now);
        assert!((rates[0].0 - 5000.0 / 1800.0).abs() < 1e-9);
        let mut b = candidate("b", 80.0);
        b.weekly_reset_at = Some(now + 3_600_000);
        assert!(router.fair_rates(&[b], now)[0].0 > rates[0].0);
    }
    #[test]
    fn replay_cost_and_compaction_honor_migration_hysteresis() {
        let mut router = SmartRouter::for_replay(0.2, false);
        let accounts = vec![candidate("a", 80.0), candidate("b", 80.0)];
        router.choose(&accounts, &context("s", 1, 200_000), 1);
        // Existing heavy conversation, with room on b. Force only load and
        // time, not a context-window change, so the old sticky rule fails.
        router.sessions.get_mut("s").unwrap().rate = Rate {
            value: 100.0,
            at: 1_800_001,
        };
        router.loads.insert(
            "a".into(),
            Rate {
                value: 100.0,
                at: 1_800_001,
            },
        );
        let mut roomy = accounts;
        roomy[1].weekly_reset_at = Some(1_800_001 + 10_000_000);
        assert_eq!(
            router
                .choose(&roomy, &context("s", 1_800_001, 1), 1_800_001)
                .0,
            0
        );
        // Compression sets transfer cost to zero, but still requires a
        // positive balance benefit and honors migration hysteresis.
        let mut req = context("s", 1_860_001, 1000);
        req.context_window_key = Some("w2".into());
        assert_eq!(router.choose(&roomy, &req, 1_860_001).0, 1);
        let migrated = router.sessions["s"].migrated;
        let flipped = vec![candidate("a", 80.0), candidate("b", 0.1)];
        req.request_id = "again".into();
        req.context_window_key = Some("w3".into());
        assert_eq!(router.choose(&flipped, &req, 1_860_002).0, 1);
        assert_eq!(router.sessions["s"].migrated, migrated);
    }
    #[test]
    fn cold_cache_can_migrate_without_changing_context_window() {
        let mut router = SmartRouter::default();
        router.choose(
            &[candidate("a", 80.0), candidate("b", 80.0)],
            &context("s", 1, 200_000),
            1,
        );
        let now = 4_200_001;
        router.sessions.get_mut("s").unwrap().rate = Rate {
            value: 100.0,
            at: now,
        };
        router.loads.insert(
            "a".into(),
            Rate {
                value: 100.0,
                at: now,
            },
        );
        let mut b = candidate("b", 80.0);
        b.weekly_reset_at = Some(now + 10_000_000);
        assert_eq!(
            router.choose(&[candidate("a", 80.0), b], &context("s", now, 1), now),
            (1, "migrate_cold_cache")
        );
    }
    #[test]
    fn recent_prior_counts_each_session_once() {
        let mut router = SmartRouter::default();
        let accounts = [candidate("a", 80.0)];
        for n in 0..20 {
            let req = context("frequent", n * 60_000 + 1, 100);
            router.choose(&accounts, &req, n * 60_000 + 1);
            router.observe(&req.request_id, 1000, 0, 200);
        }
        assert_eq!(router.samples[0].len(), 1);
    }
    #[test]
    fn gate_migration_is_immediate_and_late_usage_does_not_restore_context() {
        let mut router = SmartRouter::default();
        let req = context("s", 1, 5000);
        router.choose(&[candidate("a", 80.0)], &req, 1);
        assert_eq!(
            router
                .choose(&[candidate("b", 80.0)], &context("s", 2, 5000), 2)
                .1,
            "gate"
        );
        router.observe(&req.request_id, 80_000, 0, 200);
        assert_eq!(router.sessions["s"].account_id, "b");
        assert_eq!(router.sessions["s"].tokens, 5000);
        assert!(router.loads["a"].at(2) > 0.0);
    }
    #[test]
    fn calibration_uses_effective_tokens_and_resets_baseline() {
        let mut cal = Calibration::default();
        cal.update(10.0);
        cal.tokens_since = 400_000.0;
        cal.update(18.0);
        assert_eq!(cal.tokens_per_point, Some(50_000.0));
        cal.update(1.0);
        assert_eq!(cal.tokens_per_point, Some(50_000.0));
        assert_eq!(cal.tokens_since, 0.0);
    }
    #[test]
    fn late_rate_samples_do_not_rewind_the_clock() {
        let mut rate = Rate {
            value: 20.0,
            at: 1_200_001,
        };
        rate.add(10.0, 1);
        assert_eq!(rate.at, 1_200_001);
        assert!((rate.value - 25.0).abs() < 1e-9);
    }
    #[test]
    fn cache_survival_is_monotone_and_nonzero_for_cold_sessions() {
        assert_eq!(
            [0, 600_001, 1_800_001, 3_600_001].map(cache_heat),
            [0.95, 0.5, 0.3, 0.05]
        );
    }
}
