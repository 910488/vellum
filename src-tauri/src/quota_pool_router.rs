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
const BALANCE_HORIZON_SECONDS: f64 = 6.0 * 3600.0;
// A 5h percentage point starts at 1/12 of a weekly point. Independent
// simultaneous usage calibration replaces this prior as observations arrive.
const FIVE_HOUR_CAPACITY_RATIO: f64 = 1.0 / 12.0;
// A real rollover moves reset_at by a whole window (5h or a week). Smaller
// moves are rounding or an idle window sliding forward, and must not wipe the
// window's accumulated calibration.
const RESET_JITTER_MS: u64 = 10 * 60 * 1000;
const FIVE_HOUR_BALANCE_SECONDS: f64 = 3600.0;
const RECOVERY_FIVE_HOUR_SHARE: f64 = 0.5;
pub(crate) const DEFAULT_LAMBDA: f64 = 0.2;

#[derive(Clone, Default)]
pub(crate) struct Candidate {
    pub account_id: String,
    pub headroom: f64,
    pub capacity_prior: f64,
    pub plan_type: Option<String>,
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
    external_points: f64,
    // Recent rejected nonzero samples and their provisional external points.
    // Three consistent samples distinguish a sustained drop from one burst.
    low_samples: VecDeque<(f64, f64)>,
    reset_at: Option<u64>,
}
impl Calibration {
    fn update_window(&mut self, used: f64, prior: f64, reset_at: Option<u64>) {
        if self
            .reset_at
            .zip(reset_at)
            .is_some_and(|(old, new)| new > old.saturating_add(RESET_JITTER_MS))
        {
            self.anchor_used = None;
            self.tokens_since = 0.0;
            self.external_points = 0.0;
            self.low_samples.clear();
        }
        if reset_at.is_some() {
            self.reset_at = reset_at;
        }
        self.update(used, prior);
    }
    fn update(&mut self, used: f64, prior: f64) {
        match self.anchor_used {
            Some(anchor) if used + 0.5 < anchor => {
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
                self.low_samples.clear();
                self.external_points = 0.0;
            }
            Some(anchor)
                if used - anchor >= 2.0
                    && (self.tokens_per_point.is_some() || self.tokens_since == 0.0)
                    && self.tokens_since / (used - anchor)
                        < self.tokens_per_point.unwrap_or(prior) * 0.5 =>
            {
                let sample = self.tokens_since / (used - anchor);
                let external =
                    (used - anchor - self.tokens_since / self.tokens_per_point.unwrap_or(prior))
                        .max(0.0);
                self.external_points += external;
                if sample > 0.0 {
                    self.low_samples.push_back((sample, external));
                    if self.low_samples.len() > 3 {
                        self.low_samples.pop_front();
                    }
                    if self.low_samples.len() == 3 {
                        let mut samples =
                            self.low_samples.iter().map(|(s, _)| *s).collect::<Vec<_>>();
                        samples.sort_by(f64::total_cmp);
                        if samples[2] <= 2.0 * samples[0] {
                            self.tokens_per_point = Some(samples[1]);
                            // Only this confirmed streak is reclassified;
                            // earlier isolated external usage remains recorded.
                            self.external_points = (self.external_points
                                - self
                                    .low_samples
                                    .iter()
                                    .map(|(_, points)| points)
                                    .sum::<f64>())
                            .max(0.0);
                            self.low_samples.clear();
                        }
                    }
                } else {
                    // No pool usage cannot establish a positive capacity.
                    self.low_samples.clear();
                }
                self.anchor_used = Some(used);
                self.tokens_since = 0.0;
            }
            Some(anchor) if used - anchor >= 2.0 && self.tokens_since >= 20_000.0 => {
                self.low_samples.clear();
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
    cached_fraction: f64,
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
    plan_types: HashMap<String, Option<String>>,
    loads: HashMap<String, Rate>,
    samples: [VecDeque<(String, f64)>; 2],
    lambda: f64,
    placement_only: bool,
    pub(crate) last_admission: Vec<bool>,
    pub(crate) budget_enforced: bool,
}
impl Default for SmartRouter {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            requests: HashMap::new(),
            calibration: HashMap::new(),
            five_calibration: HashMap::new(),
            plan_types: HashMap::new(),
            loads: HashMap::new(),
            samples: Default::default(),
            lambda: DEFAULT_LAMBDA,
            placement_only: false,
            last_admission: Vec::new(),
            budget_enforced: false,
        }
    }
}
impl SmartRouter {
    pub fn diagnostic_five_capacity(
        &self,
        account: &str,
        weekly_capacity: f64,
    ) -> (f64, bool, f64, usize) {
        let cal = self.five_calibration.get(account);
        (
            cal.and_then(|c| c.tokens_per_point)
                .unwrap_or(weekly_capacity * FIVE_HOUR_CAPACITY_RATIO),
            cal.is_some_and(|c| c.tokens_per_point.is_some()),
            cal.map_or(0.0, |c| c.external_points),
            cal.map_or(0, |c| c.low_samples.len()),
        )
    }
    pub fn diagnostic_capacity(&self, account: &str, prior: f64) -> (f64, bool, f64, usize) {
        let cal = self.calibration.get(account);
        (
            cal.and_then(|c| c.tokens_per_point)
                .unwrap_or(PRIOR_TOKENS_PER_POINT * prior.max(0.01)),
            cal.is_some_and(|c| c.tokens_per_point.is_some()),
            cal.map_or(0.0, |c| c.external_points),
            cal.map_or(0, |c| c.low_samples.len()),
        )
    }
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
    pub fn update_quota_observation(&mut self, c: &Candidate) -> (f64, bool) {
        let plan = c.plan_type.as_ref().map(|p| p.trim().to_ascii_lowercase());
        if self
            .plan_types
            .get(&c.account_id)
            .is_some_and(|previous| previous != &plan)
        {
            self.calibration.remove(&c.account_id);
            self.five_calibration.remove(&c.account_id);
        }
        self.plan_types.insert(c.account_id.clone(), plan);
        let cal = self.calibration.entry(c.account_id.clone()).or_default();
        if let Some(used) = c.weekly_used {
            cal.update_window(
                used,
                PRIOR_TOKENS_PER_POINT * c.capacity_prior.max(0.01),
                c.weekly_reset_at,
            );
        }
        let capacity = cal
            .tokens_per_point
            .unwrap_or(PRIOR_TOKENS_PER_POINT * c.capacity_prior.max(0.01));
        let measured = cal.tokens_per_point.is_some();
        if let Some(used) = c.five_hour_used {
            self.five_calibration
                .entry(c.account_id.clone())
                .or_default()
                .update_window(
                    used,
                    capacity * FIVE_HOUR_CAPACITY_RATIO,
                    c.five_hour_reset_at,
                );
        }
        (capacity, measured)
    }
    fn fair_rates(&mut self, candidates: &[Candidate], now: u64) -> Vec<(f64, f64, bool)> {
        let mean_remaining = candidates
            .iter()
            .map(|c| c.weekly_headroom.unwrap_or(c.headroom).max(0.0))
            .sum::<f64>()
            / candidates.len() as f64;
        let five_remaining = candidates
            .iter()
            .filter_map(|c| c.five_hour_used.map(|u| (95.0 - u).max(0.0)))
            .collect::<Vec<_>>();
        let mean_five = if five_remaining.is_empty() {
            95.0
        } else {
            five_remaining.iter().sum::<f64>() / five_remaining.len() as f64
        };
        candidates
            .iter()
            .map(|c| {
                let (capacity, measured) = self.update_quota_observation(c);
                let remaining = c.weekly_headroom.unwrap_or(c.headroom).max(0.0);
                let until = c
                    .weekly_reset_at
                    .map_or(7.0 * 86400.0, |t| t.saturating_sub(now) as f64 / 1000.0)
                    .max(60.0);
                // Recover accumulated percentage-point skew within hours,
                // rather than letting the reset horizon postpone correction.
                let recovery =
                    (remaining - mean_remaining) * capacity / until.min(BALANCE_HORIZON_SECONDS);
                let until_week = until;
                let mut fair = remaining * capacity / until + recovery;
                if let Some(used) = c.five_hour_used {
                    let five = self
                        .five_calibration
                        .entry(c.account_id.clone())
                        .or_default();
                    let left = (95.0 - used).max(0.0);
                    let size = five
                        .tokens_per_point
                        .unwrap_or(capacity * FIVE_HOUR_CAPACITY_RATIO);
                    let until = c
                        .five_hour_reset_at
                        .map_or(18000.0, |t| t.saturating_sub(now) as f64 / 1000.0)
                        .max(60.0);
                    let five_recovery =
                        (left - mean_five) * size / until.min(FIVE_HOUR_BALANCE_SECONDS);
                    // A positive weekly correction may spend only a fraction
                    // of the sustainable full-window 5h surplus after its weekly
                    // base share, leaving spare rather than racing the most
                    // underused account straight to its reserve.
                    if recovery > 0.0 {
                        let base = remaining * capacity / until_week;
                        let sustainable = 95.0 * size / 18000.0;
                        fair = fair
                            .min(base + RECOVERY_FIVE_HOUR_SHARE * (sustainable - base).max(0.0));
                    }
                    // Recovery can tighten the 5h budget, never raise its S/T cap.
                    fair = fair.min(left * size / until + five_recovery.min(0.0));
                }
                (fair.max(0.000_001), capacity, measured)
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
        self.last_admission.clear();
        self.budget_enforced = false;
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
        let socket_incumbent = context.socket_affinity.as_ref().and_then(|affinity| {
            candidates.iter().position(|c| {
                affinity.is_account(&c.account_id)
            })
        });
        let available_incumbent = socket_incumbent.or_else(|| {
            previous.and_then(|s| candidates.iter().position(|c| c.account_id == s.account_id))
        });
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
        // Admission must include this request, rather than waiting for its
        // response to push the account over a reserve. A new realm pays full
        // input; the incumbent uses observed cache fraction and cache survival.
        let fits = |i: usize| {
            let cached = if Some(i) == available_incumbent && !compressed {
                previous.map_or(0.0, |s| s.cached_fraction) * cache_heat(gap)
            } else {
                0.0
            };
            let expected = context.estimated_input_tokens as f64 * (1.0 - 0.9 * cached);
            let weekly = candidates[i]
                .weekly_headroom
                .unwrap_or(candidates[i].headroom)
                * fair[i].1;
            let five = candidates[i]
                .five_hour_used
                .map(|used| {
                    let size = self
                        .five_calibration
                        .get(&candidates[i].account_id)
                        .and_then(|c| c.tokens_per_point)
                        .unwrap_or(fair[i].1 * FIVE_HOUR_CAPACITY_RATIO);
                    (95.0 - used).max(0.0) * size
                })
                .unwrap_or(f64::INFINITY);
            expected <= weekly.min(five)
        };
        let admission = (0..candidates.len()).map(fits).collect::<Vec<_>>();
        let any_fits = admission.iter().any(|fits| *fits);
        self.last_admission = admission.clone();
        self.budget_enforced = any_fits && socket_incumbent.is_none();
        let incumbent = available_incumbent
            .filter(|i| socket_incumbent == Some(*i) || !any_fits || admission[*i]);
        let mut best =
            incumbent.unwrap_or_else(|| (0..candidates.len()).find(|i| admission[*i]).unwrap_or(0));
        let projected =
            |i: usize| (loads[i] + if Some(i) == incumbent { 0.0 } else { rho }) / fair[i].0;
        for (i, fits) in admission.iter().enumerate() {
            if any_fits && !fits {
                continue;
            }
            if projected(i) < projected(best) * (1.0 - 1e-9) {
                best = i;
            }
        }
        let mut benefit = 0.0;
        let (selected, reason) = match incumbent {
            Some(i) if socket_incumbent == Some(i) => (i, "socket_account_affinity"),
            None => (
                best,
                if available_incumbent.is_some() && incumbent.is_none() && any_fits {
                    "gate_request_budget"
                } else if previous.is_some() {
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
                        .or(Some(fair[i].1 * FIVE_HOUR_CAPACITY_RATIO))
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
            cached_fraction: 0.0,
        });
        session.account_id = candidates[selected].account_id.clone();
        if changed || compressed {
            session.cached_fraction = 0.0;
        }
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
            format!("account={} headroom={:.3} weekly_used={:?} capacity={:.3} source={} five_used={:?} five_capacity={:.3} five_source={} external_week_est={:.3} external_five_est={:.3} L={:.3} f={:.3} u={:.3}",
                &id[..12],c.weekly_headroom.unwrap_or(c.headroom),c.weekly_used,fair[i].1,
                if fair[i].2 {"measured"} else {"prior"},c.five_hour_used,
                self.five_calibration.get(&c.account_id).and_then(|c|c.tokens_per_point).unwrap_or(fair[i].1*FIVE_HOUR_CAPACITY_RATIO),
                if self.five_calibration.get(&c.account_id).is_some_and(|c|c.tokens_per_point.is_some()) {"measured"} else {"prior"},self.calibration.get(&c.account_id).map_or(0.0, |c| c.external_points),self.five_calibration.get(&c.account_id).map_or(0.0, |c| c.external_points),loads[i],fair[i].0,loads[i]/fair[i].0)
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
                s.cached_fraction = cached as f64 / input as f64;
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
    fn enabling_pool_adopts_socket_account_and_full_context_instead_of_new_placement() {
        let mut router = SmartRouter::default();
        let candidates = [candidate("busy", 10.0), candidate("idle", 90.0)];
        let mut request = context("existing-thread", 1, 300_000);
        request.socket_affinity = Some(vellum_proxy_runtime::request::OfficialSocketAffinity {
            account_hash: vellum_proxy_runtime::request::OfficialSocketAffinity::hash_account("busy"),
            turn_in_progress: true,
            input_tokens: 300_000,
            quota_exhausted: false,
        });
        assert_eq!(
            router.choose(&candidates, &request, 1),
            (0, "socket_account_affinity")
        );
        assert_eq!(router.sessions["existing-thread"].tokens, 300_000);
        router.observe(&request.request_id, 300_000, 297_000, 200);
        request.socket_affinity.as_mut().unwrap().turn_in_progress = false;
        request.request_id = "next-root-turn".into();
        assert_eq!(
            router.choose(&candidates, &request, 2).0,
            0,
            "a usable account retains its warm context at the next turn boundary"
        );
        assert_eq!(
            router.choose(&candidates[1..], &request, 3).0,
            0,
            "an unavailable account can move at a boundary"
        );
        assert_eq!(router.sessions["existing-thread"].account_id, "idle");
    }
    #[test]
    fn accumulated_weekly_skew_changes_placement_before_new_usage_arrives() {
        let mut router = SmartRouter::default();
        let mut high = candidate("high", 70.0);
        high.weekly_headroom = Some(35.0);
        // The old reset-only rate favors the busy measured account over an
        // underused account still on its smaller capacity prior.
        router.calibration.insert(
            "high".into(),
            Calibration {
                tokens_per_point: Some(600_000.0),
                ..Default::default()
            },
        );
        let mut low = candidate("low", 80.0);
        low.weekly_headroom = Some(80.0);
        // An unloaded but heavily spent rank-one account must not win ties.
        assert_eq!(router.choose(&[high, low], &context("new", 1, 100), 1).0, 1);
    }
    #[test]
    fn five_hour_prior_caps_rate_before_any_capacity_measurement() {
        let mut router = SmartRouter::default();
        let mut capped = candidate("a", 80.0);
        capped.weekly_reset_at = Some(3_600_001);
        capped.five_hour_used = Some(10.0);
        capped.five_hour_reset_at = Some(18_000_001);
        let mut uncapped = capped.clone();
        uncapped.account_id = "b".into();
        uncapped.five_hour_used = None;
        let mut busy = capped.clone();
        busy.account_id = "c".into();
        busy.five_hour_used = Some(50.0);
        let fair = router.fair_rates(&[capped, uncapped, busy], 1);
        assert!(fair[0].0 < fair[1].0);
        assert!((fair[0].0 - 85.0 * 10_000.0 / 12.0 / 18000.0).abs() < 1e-8);
        assert!(router.five_calibration["a"].tokens_per_point.is_none());
    }
    #[test]
    fn external_usage_preserves_weekly_and_five_hour_capacity() {
        for capacity in [600_000.0, 50_000.0] {
            let mut cal = Calibration::default();
            cal.update(10.0, capacity);
            cal.tokens_since = 2.0 * capacity;
            cal.update(12.0, capacity);
            assert_eq!(cal.tokens_per_point, Some(capacity));
            // Two pool points plus twenty externally consumed points.
            cal.tokens_since = 2.0 * capacity;
            cal.update(34.0, capacity);
            assert!(cal.tokens_per_point.unwrap() >= 0.9 * capacity);
            assert!((cal.external_points - 20.0).abs() < 1e-9);
            assert_eq!(cal.anchor_used, Some(34.0));
            assert_eq!(cal.tokens_since, 0.0);
            // Discarding the contaminated anchor must allow normal learning.
            cal.tokens_since = 2.0 * capacity;
            cal.update(36.0, capacity);
            assert_eq!(cal.tokens_per_point, Some(capacity));
            cal.update(0.0, capacity);
            assert_eq!(cal.tokens_per_point, Some(capacity));
        }
    }
    #[test]
    fn sustained_twenty_fold_capacity_drop_replaces_old_estimate() {
        for capacity in [600_000.0, 50_000.0] {
            let mut cal = Calibration {
                anchor_used: Some(10.0),
                tokens_per_point: Some(capacity),
                external_points: 7.0,
                ..Default::default()
            };
            for n in 1..=3 {
                cal.tokens_since = 2.0 * capacity / 20.0;
                cal.update(10.0 + 2.0 * n as f64, capacity);
                assert_eq!(
                    cal.tokens_per_point,
                    Some(if n == 3 { capacity / 20.0 } else { capacity })
                );
            }
            assert!((cal.external_points - 7.0).abs() < 1e-9);
            assert!(cal.low_samples.is_empty());
        }
    }

    #[test]
    fn normal_sample_and_reset_break_low_capacity_streak() {
        let mut cal = Calibration {
            anchor_used: Some(0.0),
            tokens_per_point: Some(100_000.0),
            ..Default::default()
        };
        for used in [2.0, 4.0] {
            cal.tokens_since = 20_000.0;
            cal.update(used, 100_000.0);
        }
        cal.tokens_since = 200_000.0;
        cal.update(6.0, 100_000.0);
        assert!(cal.low_samples.is_empty());
        cal.tokens_since = 20_000.0;
        cal.update(8.0, 100_000.0);
        assert_eq!(cal.tokens_per_point, Some(100_000.0));
        cal.update(0.0, 100_000.0);
        assert!(cal.low_samples.is_empty());
    }

    const FIVE_HOUR_MS: u64 = 5 * 60 * 60 * 1000;

    #[test]
    fn reset_timestamp_jitter_keeps_window_calibration() {
        let mut cal = Calibration {
            anchor_used: Some(20.0),
            tokens_since: 30_000.0,
            tokens_per_point: Some(10_000.0),
            external_points: 12.0,
            low_samples: VecDeque::new(),
            reset_at: Some(1_000_000),
        };
        cal.update_window(20.0, 10_000.0, Some(1_000_000 + 1_000));
        assert_eq!(cal.anchor_used, Some(20.0));
        assert_eq!(cal.tokens_since, 30_000.0);
        assert_eq!(cal.external_points, 12.0);
    }

    #[test]
    fn quota_reset_clears_window_usage_but_preserves_learned_capacity() {
        for reset_by_time in [false, true] {
            let mut cal = Calibration {
                anchor_used: Some(20.0),
                tokens_since: 30_000.0,
                tokens_per_point: Some(10_000.0),
                external_points: 12.0,
                low_samples: VecDeque::from([(1000.0, 2.0)]),
                reset_at: Some(100),
            };
            // A rollover can be missed between snapshots: the new window
            // may already have higher usage, so percentage decrease alone
            // is insufficient to recognize a reset.
            cal.update_window(
                if reset_by_time { 25.0 } else { 0.0 },
                10_000.0,
                Some(if reset_by_time {
                    100 + FIVE_HOUR_MS
                } else {
                    100
                }),
            );
            assert_eq!(cal.external_points, 0.0);
            assert_eq!(cal.tokens_since, 0.0);
            assert!(cal.low_samples.is_empty());
            assert_eq!(cal.tokens_per_point, Some(10_000.0));
        }
        let mut router = SmartRouter::default();
        let mut gated = candidate("gated", 0.0);
        gated.weekly_used = Some(100.0);
        gated.weekly_headroom = Some(0.0);
        gated.five_hour_used = Some(97.0);
        gated.weekly_reset_at = Some(100);
        gated.five_hour_reset_at = Some(100);
        router.update_quota_observation(&gated);
        for cal in [&mut router.calibration, &mut router.five_calibration] {
            cal.get_mut("gated").unwrap().external_points = 12.0;
        }
        // Even a newly exhausted post-reset snapshot must reconcile its
        // window state before eligibility filtering skips this account.
        gated.weekly_reset_at = Some(100 + FIVE_HOUR_MS);
        gated.five_hour_reset_at = Some(100 + FIVE_HOUR_MS);
        router.update_quota_observation(&gated);
        for cal in [&router.calibration, &router.five_calibration] {
            assert_eq!(cal["gated"].external_points, 0.0);
        }
    }

    #[test]
    fn unknown_plan_remains_eligible_and_does_not_reset_repeatedly() {
        let mut router = SmartRouter::default();
        let c = candidate("a", 90.0);
        router.fair_rates(std::slice::from_ref(&c), 1);
        router.calibration.get_mut("a").unwrap().tokens_per_point = Some(30_000.0);
        router.fair_rates(std::slice::from_ref(&c), 2);
        assert_eq!(router.calibration["a"].tokens_per_point, Some(30_000.0));
        assert_eq!(router.choose(&[c], &context("new", 3, 100), 3).0, 0);
    }

    #[test]
    fn inconsistent_external_samples_do_not_reset_capacity() {
        let mut cal = Calibration {
            anchor_used: Some(0.0),
            tokens_per_point: Some(100_000.0),
            ..Default::default()
        };
        for (n, sample) in [1000.0, 30_000.0, 1000.0, 30_000.0].into_iter().enumerate() {
            cal.tokens_since = 2.0 * sample;
            cal.update(2.0 * (n + 1) as f64, 100_000.0);
        }
        assert_eq!(cal.tokens_per_point, Some(100_000.0));
        assert!(cal.external_points > 0.0);
        assert_eq!(cal.low_samples.len(), 3);
    }

    #[test]
    fn plan_change_resets_weekly_and_five_hour_calibration_to_prior() {
        let mut router = SmartRouter::default();
        let mut c = candidate("a", 90.0);
        c.plan_type = Some("Pro".into());
        c.capacity_prior = 20.0;
        c.weekly_used = Some(10.0);
        c.five_hour_used = Some(10.0);
        router.fair_rates(std::slice::from_ref(&c), 1);
        for cal in [&mut router.calibration, &mut router.five_calibration] {
            cal.get_mut("a").unwrap().tokens_per_point = Some(600_000.0);
            cal.get_mut("a").unwrap().external_points = 12.0;
        }
        c.plan_type = Some(" pro ".into());
        router.fair_rates(std::slice::from_ref(&c), 2);
        assert_eq!(router.calibration["a"].tokens_per_point, Some(600_000.0));
        c.plan_type = Some("Plus".into());
        c.capacity_prior = 1.0;
        let rates = router.fair_rates(&[c], 3);
        assert_eq!(rates[0].1, PRIOR_TOKENS_PER_POINT);
        for cal in [&router.calibration, &router.five_calibration] {
            assert_eq!(cal["a"].tokens_per_point, None);
            assert_eq!(cal["a"].tokens_since, 0.0);
            assert_eq!(cal["a"].external_points, 0.0);
        }
    }

    #[test]
    fn first_capacity_sample_can_replace_an_overstated_prior() {
        let mut cal = Calibration::default();
        cal.update(10.0, 200_000.0);
        cal.tokens_since = 40_000.0;
        cal.update(18.0, 200_000.0);
        assert_eq!(cal.tokens_per_point, Some(5000.0));
        assert_eq!(cal.external_points, 0.0);
    }
    #[test]
    fn external_jump_without_pool_tokens_advances_anchor_not_capacity() {
        let mut cal = Calibration {
            anchor_used: Some(10.0),
            tokens_per_point: Some(600_000.0),
            ..Default::default()
        };
        cal.update(30.0, 200_000.0);
        assert_eq!(cal.tokens_per_point, Some(600_000.0));
        assert_eq!(cal.external_points, 20.0);
        assert_eq!(cal.anchor_used, Some(30.0));
    }
    #[test]
    fn unequal_start_recovery_leaves_sustainable_five_hour_spare() {
        let mut router = SmartRouter::default();
        let accounts = [10.0, 35.0, 60.0, 80.0].map(|used| {
            let id = used.to_string();
            router.calibration.insert(
                id.clone(),
                Calibration {
                    tokens_per_point: Some(1_200_000.0),
                    ..Default::default()
                },
            );
            router.five_calibration.insert(
                id.clone(),
                Calibration {
                    tokens_per_point: Some(50_000.0),
                    ..Default::default()
                },
            );
            Candidate {
                account_id: id,
                headroom: 95.0 - used,
                weekly_headroom: Some(95.0 - used),
                weekly_used: Some(used),
                weekly_reset_at: Some(604_800_001),
                five_hour_used: Some(0.0),
                five_hour_reset_at: Some(18_000_001),
                capacity_prior: 20.0,
                plan_type: None,
            }
        });
        let fair = router.fair_rates(&accounts, 1)[0].0;
        let base = 85.0 * 1_200_000.0 / 604_800.0;
        let sustainable = 95.0 * 50_000.0 / 18000.0;
        assert!((fair - base - RECOVERY_FIVE_HOUR_SHARE * (sustainable - base)).abs() < 1e-8);
        assert!(
            fair * 18000.0 < 95.0 * 50_000.0,
            "weekly recovery must leave spare below the 5h reserve"
        );
        // An absent 5h window imposes no artificial cap.
        let mut without = accounts;
        for c in &mut without {
            c.five_hour_used = None;
        }
        assert!(router.fair_rates(&without, 1)[0].0 > sustainable);
    }
    #[test]
    fn request_budget_moves_before_five_hour_reserve_is_crossed() {
        let mut router = SmartRouter::default();
        let mut accounts = [candidate("a", 80.0), candidate("b", 80.0)];
        for c in &mut accounts {
            c.five_hour_used = Some(0.0);
            router.five_calibration.insert(
                c.account_id.clone(),
                Calibration {
                    tokens_per_point: Some(50_000.0),
                    ..Default::default()
                },
            );
        }
        let first = context("s", 1, 1000);
        assert_eq!(router.choose(&accounts, &first, 1).0, 0);
        router.observe(&first.request_id, 100_000, 100_000, 200);
        accounts[0].five_hour_used = Some(94.9);
        router.sessions.get_mut("s").unwrap().migrated = Some(1);
        // Its next hot-cache request needs about 14.5k effective tokens,
        // exceeding 5k left. A normal migration is still in hysteresis.
        assert_eq!(
            router.choose(&accounts, &context("s", 2, 100_000), 2),
            (1, "gate_request_budget")
        );
        assert_eq!(router.sessions["s"].cached_fraction, 0.0);
    }
    #[test]
    fn request_budget_retains_cache_affinity_and_handles_no_fitting_account() {
        let mut router = SmartRouter::default();
        let mut accounts = [candidate("a", 80.0), candidate("b", 80.0)];
        for c in &mut accounts {
            c.five_hour_used = Some(0.0);
            router.five_calibration.insert(
                c.account_id.clone(),
                Calibration {
                    tokens_per_point: Some(50_000.0),
                    ..Default::default()
                },
            );
        }
        let first = context("s", 1, 1000);
        router.choose(&accounts, &first, 1);
        router.observe(&first.request_id, 100_000, 100_000, 200);
        accounts[0].five_hour_used = Some(94.4); // 30k left: cached fits, full does not.
        assert_eq!(router.choose(&accounts, &context("s", 2, 100_000), 2).0, 0);
        // If every candidate is too small, do not repeatedly force a realm
        // change from an estimate alone. Real gates retain their old fallback.
        accounts[1].five_hour_used = Some(94.4);
        assert_eq!(
            router.choose(&accounts, &context("s", 3, 1_000_000), 3).0,
            0
        );
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
        cal.update(10.0, 10_000.0);
        cal.tokens_since = 400_000.0;
        cal.update(18.0, 10_000.0);
        assert_eq!(cal.tokens_per_point, Some(50_000.0));
        cal.update(1.0, 10_000.0);
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
