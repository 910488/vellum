//! Read-only local replay. No request identities or raw rows are exported.
//!
//! cargo run -p vellum-quota-pool-replay -- <usage.sqlite3> [model.json]
//! The optional JSON array contains synthetic capacities/reset offsets only.
#[allow(dead_code)]
#[path = "quota_pool_legacy.rs"]
mod legacy;
#[allow(dead_code)]
#[path = "../../../src-tauri/src/quota_pool_router.rs"]
mod router;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use vellum_proxy_runtime::official_auth::OfficialRoutingContext;

#[derive(Clone, Deserialize)]
struct Model {
    weekly_capacity: f64,
    five_hour_capacity: Option<f64>,
    initial_weekly_used: f64,
    initial_five_hour_used: f64,
    weekly_floor: f64,
    weekly_reset_seconds: u64,
    five_hour_reset_seconds: u64,
    plan_prior: f64,
}
struct Account {
    model: Model,
    weekly: f64,
    five: f64,
    weekly_reset: u64,
    five_reset: u64,
    at: u64,
    reserve_ms: u64,
}
impl Account {
    fn new(model: Model, at: u64) -> Self {
        Self {
            weekly: model.initial_weekly_used,
            five: model.initial_five_hour_used,
            weekly_reset: at + model.weekly_reset_seconds * 1000,
            five_reset: at + model.five_hour_reset_seconds * 1000,
            model,
            at,
            reserve_ms: 0,
        }
    }
    fn advance(&mut self, now: u64) {
        if self.model.five_hour_capacity.is_some() {
            if self.five >= 95.0 {
                self.reserve_ms += now.min(self.five_reset).saturating_sub(self.at);
            }
            while now >= self.five_reset {
                self.five = 0.0;
                self.five_reset += 18_000_000;
            }
        }
        while now >= self.weekly_reset {
            self.weekly = 0.0;
            self.weekly_reset += 604_800_000;
        }
        self.at = now;
    }
    fn candidate(&self, index: usize) -> router::Candidate {
        let week = (100.0 - self.weekly - self.model.weekly_floor).max(0.0);
        let five = self
            .model
            .five_hour_capacity
            .map_or(100.0, |_| (95.0 - self.five).max(0.0));
        router::Candidate {
            account_id: format!("synthetic-{index}"),
            headroom: (week / (100.0 - self.model.weekly_floor) * 100.0).min(five / 95.0 * 100.0),
            capacity_prior: self.model.plan_prior,
            weekly_used: Some(self.weekly),
            weekly_headroom: Some(week),
            weekly_reset_at: Some(self.weekly_reset),
            five_hour_used: self.model.five_hour_capacity.map(|_| self.five),
            five_hour_reset_at: self.model.five_hour_capacity.map(|_| self.five_reset),
        }
    }
    fn spend(&mut self, tokens: f64) {
        self.weekly += 100.0 * tokens / self.model.weekly_capacity;
        if let Some(cap) = self.model.five_hour_capacity {
            self.five += 100.0 * tokens / cap;
        }
    }
}
struct Row {
    at: u64,
    session: String,
    model: String,
    input: u64,
    cached: u64,
    original_account: String,
}
#[derive(Serialize)]
struct Point {
    elapsed_hours: f64,
    used_std: f64,
    used_spread: f64,
}
#[derive(Serialize)]
struct ResultRow {
    strategy: String,
    lambda: Option<f64>,
    effective_tokens: f64,
    used_std: f64,
    used_spread: f64,
    migrations: u64,
    reserve_hours: f64,
    all_gated_requests: u64,
    timeline: Vec<Point>,
}
fn dispersion(accounts: &[Account]) -> (f64, f64) {
    let values = accounts
        .iter()
        .map(|a| a.weekly / (100.0 - a.model.weekly_floor) * 100.0)
        .collect::<Vec<_>>();
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    (
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt(),
        values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            - values.iter().copied().fold(f64::INFINITY, f64::min),
    )
}
fn simulate(rows: &[Row], models: &[Model], kind: &str, lambda: f64) -> ResultRow {
    let first = rows[0].at;
    let mut accounts = models
        .iter()
        .cloned()
        .map(|m| Account::new(m, first))
        .collect::<Vec<_>>();
    let mut smart = router::SmartRouter::for_replay(lambda, kind == "placement_only");
    let mut old = legacy::SmartRouter::default();
    let mut previous = HashMap::<String, usize>::new();
    let mut original_context = HashMap::<String, (u64, String)>::new();
    let mut windows = HashMap::<String, u64>::new();
    let mut effective_tokens = 0.0;
    let mut migrations = 0;
    let mut all_gated = 0;
    let mut timeline = Vec::new();
    let mut timeline_at = first;
    for (n, row) in rows.iter().enumerate() {
        for account in &mut accounts {
            account.advance(row.at);
        }
        let mut eligible = (0..accounts.len())
            .filter(|i| {
                accounts[*i].weekly < 100.0 - accounts[*i].model.weekly_floor
                    && (accounts[*i].model.five_hour_capacity.is_none() || accounts[*i].five < 95.0)
            })
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            // Production's manual fallback still attempts a valid account;
            // report these attempts explicitly rather than dropping demand.
            all_gated += 1;
            eligible.push(
                (0..accounts.len())
                    .min_by(|a, b| accounts[*a].weekly.total_cmp(&accounts[*b].weekly))
                    .unwrap(),
            );
        }
        let window = windows.entry(row.session.clone()).or_default();
        if original_context
            .get(&row.session)
            .is_some_and(|(last, model)| model == &row.model && row.input < last / 2)
        {
            *window += 1;
        }
        original_context.insert(row.session.clone(), (row.input, row.model.clone()));
        // usage.sqlite3 has no context-window IDs. A >50% same-model input
        // reduction is an explicitly documented compaction proxy.
        let key = if kind == "legacy" {
            format!("{}:{}", row.session, row.model)
        } else {
            row.session.clone()
        };
        let context = OfficialRoutingContext {
            session_key: Some(key),
            request_id: format!("replay-{n}"),
            context_window_key: Some(window.to_string()),
            estimated_input_tokens: if previous.contains_key(&row.session) {
                row.input
            } else {
                row.input.min(4000)
            },
            context_window: Some(400_000),
        };
        let candidates = eligible
            .iter()
            .map(|i| accounts[*i].candidate(*i))
            .collect::<Vec<_>>();
        let selected = if kind == "legacy" {
            let c = candidates
                .iter()
                .map(|c| legacy::Candidate {
                    account_id: c.account_id.clone(),
                    headroom: c.headroom,
                    capacity_prior: c.capacity_prior,
                    weekly_used: c.weekly_used,
                })
                .collect::<Vec<_>>();
            old.choose(&c, &context, row.at).0
        } else {
            smart.choose(&candidates, &context, row.at).0
        };
        let account = eligible[selected];
        let moved = previous
            .get(&row.session)
            .is_some_and(|old| *old != account);
        migrations += u64::from(moved);
        let cached = if moved { 0 } else { row.cached.min(row.input) };
        let effective = (row.input - cached) as f64 + 0.1 * cached as f64;
        accounts[account].spend(effective);
        effective_tokens += effective;
        if kind == "legacy" {
            old.observe(&context.request_id, row.input, cached, 200);
        } else {
            smart.observe(&context.request_id, row.input, cached, 200);
        }
        previous.insert(row.session.clone(), account);
        if row.at >= timeline_at || n + 1 == rows.len() {
            let (std, spread) = dispersion(&accounts);
            timeline.push(Point {
                elapsed_hours: (row.at - first) as f64 / 3_600_000.0,
                used_std: std,
                used_spread: spread,
            });
            timeline_at = row.at + 3_600_000;
        }
    }
    let (used_std, used_spread) = dispersion(&accounts);
    ResultRow {
        strategy: kind.into(),
        lambda: (kind == "full").then_some(lambda),
        effective_tokens,
        used_std,
        used_spread,
        migrations,
        reserve_hours: accounts
            .iter()
            .map(|a| a.reserve_ms as f64 / 3_600_000.0)
            .sum(),
        all_gated_requests: all_gated,
        timeline,
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let path = args
        .get(1)
        .ok_or("usage: quota_pool_replay <usage.sqlite3> [synthetic-model.json]")?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.execute_batch("BEGIN DEFERRED TRANSACTION")?;
    let end: i64 = conn.query_row("SELECT MAX(created_at) FROM request_usage", [], |r| {
        r.get(0)
    })?;
    let mut stmt=conn.prepare("SELECT created_at,conversation_identity,model,input_tokens,COALESCE(cached_input_tokens,0),COALESCE(execution_account_hash,'') FROM request_usage WHERE route_id='openai-official' AND status=200 AND input_tokens>0 AND conversation_identity IS NOT NULL AND created_at>? ORDER BY created_at,id")?;
    let rows = stmt
        .query_map([end - 604800], |r| {
            let model: String = r.get(2)?;
            Ok(Row {
                at: r.get::<_, u64>(0)? * 1000,
                session: format!(
                    "{}:{}",
                    if model == "codex-auto-review" {
                        "review"
                    } else {
                        "main"
                    },
                    r.get::<_, String>(1)?
                ),
                model,
                input: r.get(3)?,
                cached: r.get(4)?,
                original_account: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Err("No replayable successful Official requests".into());
    }
    let models: Vec<Model> = if let Some(config) = args.get(2) {
        serde_json::from_slice(&std::fs::read(config)?)?
    } else {
        (0..4)
            .map(|_| Model {
                weekly_capacity: 60_000_000.0,
                five_hour_capacity: Some(5_000_000.0),
                initial_weekly_used: 20.0,
                initial_five_hour_used: 0.0,
                weekly_floor: 5.0,
                weekly_reset_seconds: 604800,
                five_hour_reset_seconds: 18000,
                plan_prior: 20.0,
            })
            .collect()
    };
    if models.is_empty()
        || models.iter().any(|m| {
            !m.weekly_capacity.is_finite()
                || m.weekly_capacity <= 0.0
                || m.weekly_floor >= 100.0
                || m.five_hour_capacity
                    .is_some_and(|c| !c.is_finite() || c <= 0.0)
        })
    {
        return Err("Invalid synthetic quota model".into());
    }
    let mut last = HashMap::<(&str, &str), &Row>::new();
    let mut short = 0;
    let mut low = 0;
    for row in &rows {
        if let Some(prior) = last.insert((&row.session, &row.model), row) {
            if row.at.saturating_sub(prior.at) <= 30_000
                && row.original_account == prior.original_account
                && !row.original_account.is_empty()
            {
                short += 1;
                if row.cached as f64 / (prior.input as f64) < 0.5 {
                    low += 1;
                }
            }
        }
    }
    let mut results = vec![
        simulate(&rows, &models, "legacy", 0.0),
        simulate(&rows, &models, "placement_only", 0.0),
    ];
    for lambda in [0.05, 0.1, 0.2, 0.5, 1.0] {
        results.push(simulate(&rows, &models, "full", lambda));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"requests":rows.len(),"same_account_within_30s":short,
        "low_reuse_within_30s":low,"low_reuse_percent":100.0*low as f64/(short.max(1) as f64),"results":results})
        )?
    );
    Ok(())
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    fn model(capacity: f64) -> Model {
        Model {
            weekly_capacity: capacity,
            five_hour_capacity: None,
            initial_weekly_used: 0.0,
            initial_five_hour_used: 0.0,
            weekly_floor: 5.0,
            weekly_reset_seconds: 604800,
            five_hour_reset_seconds: 18000,
            plan_prior: 1.0,
        }
    }
    #[test]
    fn moving_accounts_replays_uncached_input_in_every_comparison() {
        let rows = [
            Row {
                at: 1,
                session: "main:s".into(),
                model: "sol".into(),
                input: 1_000_000,
                cached: 900_000,
                original_account: "a".into(),
            },
            Row {
                at: 60_001,
                session: "main:s".into(),
                model: "sol".into(),
                input: 1_000_000,
                cached: 900_000,
                original_account: "a".into(),
            },
        ];
        for kind in ["legacy", "placement_only", "full"] {
            let result = simulate(&rows, &[model(100_000.0), model(100_000.0)], kind, 0.1);
            assert_eq!(result.migrations, 1);
            assert_eq!(result.effective_tokens, 1_190_000.0);
        }
    }
    #[test]
    fn reserve_duration_stops_at_reset_even_across_long_idle_gaps() {
        let mut m = model(1_000_000.0);
        m.five_hour_capacity = Some(100_000.0);
        m.five_hour_reset_seconds = 3600;
        let mut account = Account::new(m, 1);
        account.spend(100_000.0);
        account.advance(7_200_001);
        assert_eq!(account.reserve_ms, 3_600_000);
        assert_eq!(account.five, 0.0);
    }
}
