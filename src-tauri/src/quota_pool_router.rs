//! Session-aware quota allocation. Scores are heuristics in percentage points,
//! not a conversion between tokens and subscription allowance. No prompts,
//! credentials, or raw conversation identities are retained here.

use std::collections::HashMap;
use vellum_proxy_runtime::official_auth::OfficialRoutingContext;

const IDLE_MS: u64 = 30 * 60 * 1000;
const MAX_SESSIONS: usize = 1024;
const MAX_REQUESTS: usize = 4096;

pub(crate) struct Candidate {
    pub account_id: String,
    pub headroom: f64,
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
        let total = loads
            .iter()
            .fold(tokens, |sum, load| sum.saturating_add(*load))
            .max(1) as f64;
        let score = |index: usize| {
            candidates[index].headroom - 20.0 * (loads[index].saturating_add(tokens) as f64 / total)
        };
        let mut best = 0;
        for index in 1..candidates.len() {
            if score(index) > score(best) + 0.001 {
                best = index;
            }
        }
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
                if score(best) - score(index) > switching_cost {
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
    fn accounts(a: f64, b: f64) -> Vec<Candidate> {
        vec![
            Candidate {
                account_id: "a".into(),
                headroom: a,
            },
            Candidate {
                account_id: "b".into(),
                headroom: b,
            },
        ]
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
        let only_b = vec![Candidate {
            account_id: "b".into(),
            headroom: 70.0,
        }];
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
}
