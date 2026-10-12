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

#[allow(dead_code)]
#[path = "quota_pool_previous.rs"]
mod previous_router;

#[allow(dead_code)]
#[path = "quota_pool_review08.rs"]
mod review08;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use vellum_proxy_runtime::official_auth::OfficialRoutingContext;

#[derive(Clone, Deserialize, Serialize)]
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
#[derive(Clone)]
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
}
#[derive(Serialize)]
struct ResultRow {
    strategy: String,
    lambda: Option<f64>,
    effective_tokens: f64,
    used_std: f64,
    used_spread: f64,
    time_mean_sd: f64,
    time_p90_sd: f64,
    diagnostics: Vec<Decision>,
    migrations: u64,
    reserve_hours: f64,
    all_gated_requests: u64,
    timeline: Vec<Point>,
    quota_events: Vec<serde_json::Value>,
    calibration_snapshots: Vec<serde_json::Value>,
}
struct DecisionLogger(std::sync::Mutex<String>);
static DECISION_LOGGER: DecisionLogger = DecisionLogger(std::sync::Mutex::new(String::new()));
impl log::Log for DecisionLogger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        let line = record.args().to_string();
        if line.starts_with("[QuotaPool] strategy=balanced") {
            *self.0.lock().unwrap() = line;
        }
    }
    fn flush(&self) {}
}
#[derive(Serialize)]
struct Decision {
    decision_log: String,
    elapsed_hours: f64,
    reason: String,
    selected: usize,
    moved: bool,
    sd_increase: f64,
    before_used: Vec<f64>,
    candidate_headroom: Vec<f64>,
}
#[derive(Default)]
struct TimeDispersion {
    segments: Vec<(f64, u64)>,
}
impl TimeDispersion {
    fn add(&mut self, sd: f64, duration: u64) {
        if duration > 0 {
            self.segments.push((sd, duration));
        }
    }
    fn metrics(&self) -> (f64, f64) {
        let duration = self.segments.iter().map(|(_, dt)| *dt).sum::<u64>();
        if duration == 0 {
            return (0.0, 0.0);
        }
        let mean = self
            .segments
            .iter()
            .map(|(sd, dt)| sd * *dt as f64)
            .sum::<f64>()
            / duration as f64;
        let mut ordered = self.segments.clone();
        ordered.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut cumulative = 0u64;
        let p90 = ordered
            .into_iter()
            .find_map(|(sd, dt)| {
                cumulative += dt;
                (cumulative as f64 >= duration as f64 * 0.9).then_some(sd)
            })
            .unwrap_or(0.0);
        (mean, p90)
    }
}
fn advance_accounts(accounts: &mut [Account], from: u64, now: u64, metrics: &mut TimeDispersion) {
    let mut at = from;
    while at < now {
        let next = accounts
            .iter()
            .flat_map(|a| {
                [
                    Some(a.weekly_reset),
                    a.model.five_hour_capacity.map(|_| a.five_reset),
                ]
            })
            .flatten()
            .filter(|t| *t > at)
            .min()
            .unwrap_or(now)
            .min(now);
        metrics.add(dispersion(accounts).0, next - at);
        for a in accounts.iter_mut() {
            a.advance(next);
        }
        at = next;
    }
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
    simulate_events(rows, models, kind, lambda, &[])
}
#[derive(Clone, Serialize)]
struct ExternalEvent {
    at: u64,
    account: usize,
    weekly_points: f64,
}
fn simulate_events(
    rows: &[Row],
    models: &[Model],
    kind: &str,
    lambda: f64,
    events: &[ExternalEvent],
) -> ResultRow {
    let first = rows[0].at;
    let mut accounts = models
        .iter()
        .cloned()
        .map(|m| Account::new(m, first))
        .collect::<Vec<_>>();
    let mut smart = router::SmartRouter::for_replay(lambda, kind == "placement_only");
    let mut baseline = review08::SmartRouter::for_replay(lambda, false);
    let mut old = legacy::SmartRouter::default();
    let mut prior = previous_router::SmartRouter::for_replay(lambda, kind == "previous_placement");
    let mut previous = HashMap::<String, usize>::new();
    let mut original_context = HashMap::<String, (u64, String)>::new();
    let mut windows = HashMap::<String, u64>::new();
    let mut effective_tokens = 0.0;
    let mut migrations = 0;
    let mut all_gated = 0;

    let mut time_metrics = TimeDispersion::default();
    let mut last_at = first;
    let mut diagnostics = Vec::new();
    let mut quota_events = Vec::new();
    let mut calibration_snapshots = Vec::new();
    let mut snapshot_hour = u64::MAX;
    let mut event_index = 0;
    for (n, row) in rows.iter().enumerate() {
        while event_index < events.len() && events[event_index].at <= row.at {
            let e = &events[event_index];
            advance_accounts(&mut accounts, last_at, e.at, &mut time_metrics);
            last_at = e.at;
            let a = &mut accounts[e.account];
            a.spend(e.weekly_points * a.model.weekly_capacity / 100.0);
            // An upstream 5h observation is bounded at 100%; external spend
            // includes activity outside the replay's pool, not billed pool input.
            a.five = a.five.min(100.0);
            quota_events.push(serde_json::json!({"event":"external_burst", "hour":(e.at-first) as f64/3_600_000.0,"account":e.account,"weekly_points":e.weekly_points}));
            event_index += 1;
        }
        advance_accounts(&mut accounts, last_at, row.at, &mut time_metrics);
        last_at = row.at;
        let before_sd = dispersion(&accounts).0;
        let before_used = accounts.iter().map(|a| a.weekly).collect::<Vec<_>>();
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
        let (selected, reason) = if kind == "legacy" {
            let c = candidates
                .iter()
                .map(|c| legacy::Candidate {
                    account_id: c.account_id.clone(),
                    headroom: c.headroom,
                    capacity_prior: c.capacity_prior,
                    weekly_used: c.weekly_used,
                })
                .collect::<Vec<_>>();
            old.choose(&c, &context, row.at)
        } else if kind == "review08" {
            let c = candidates
                .iter()
                .map(|c| review08::Candidate {
                    account_id: c.account_id.clone(),
                    headroom: c.headroom,
                    capacity_prior: c.capacity_prior,
                    weekly_used: c.weekly_used,
                    weekly_headroom: c.weekly_headroom,
                    weekly_reset_at: c.weekly_reset_at,
                    five_hour_used: c.five_hour_used,
                    five_hour_reset_at: c.five_hour_reset_at,
                })
                .collect::<Vec<_>>();
            baseline.choose(&c, &context, row.at)
        } else if kind.starts_with("previous_") {
            let c = candidates
                .iter()
                .map(|c| previous_router::Candidate {
                    account_id: c.account_id.clone(),
                    headroom: c.headroom,
                    capacity_prior: c.capacity_prior,
                    weekly_used: c.weekly_used,
                    weekly_headroom: c.weekly_headroom,
                    weekly_reset_at: c.weekly_reset_at,
                    five_hour_used: c.five_hour_used,
                    five_hour_reset_at: c.five_hour_reset_at,
                })
                .collect::<Vec<_>>();
            prior.choose(&c, &context, row.at)
        } else {
            smart.choose(&candidates, &context, row.at)
        };
        let hour = (row.at - first) / 3_600_000;
        if snapshot_hour != hour && kind != "legacy" {
            snapshot_hour = hour;
            calibration_snapshots.push(serde_json::json!({"hour":hour,"decision_log":DECISION_LOGGER.0.lock().unwrap().clone()}));
        }
        let account = eligible[selected];
        let moved = previous
            .get(&row.session)
            .is_some_and(|old| *old != account);
        migrations += u64::from(moved);
        let cached = if moved { 0 } else { row.cached.min(row.input) };
        let effective = (row.input - cached) as f64 + 0.1 * cached as f64;
        let before_five = accounts[account].five;
        accounts[account].spend(effective);
        if before_five < 95.0 && accounts[account].five >= 95.0 {
            quota_events.push(serde_json::json!({"event":"pool_reserve", "hour":(row.at-first) as f64/3_600_000.0,"account":account,"weekly_used":accounts[account].weekly,"five_used":accounts[account].five,"decision_log": if kind=="legacy" {String::new()} else {DECISION_LOGGER.0.lock().unwrap().clone()}}));
        }
        if event_index > 0
            && row.at.saturating_sub(events[event_index - 1].at) <= 600_000
            && n % 10 == 0
        {
            quota_events.push(serde_json::json!({"event":"post_external_calibration","hour":(row.at-first) as f64/3_600_000.0,"decision_log":if kind=="legacy" {String::new()} else {DECISION_LOGGER.0.lock().unwrap().clone()}}));
        }
        effective_tokens += effective;
        if kind == "legacy" {
            old.observe(&context.request_id, row.input, cached, 200);
        } else if kind == "review08" {
            baseline.observe(&context.request_id, row.input, cached, 200);
        } else if kind.starts_with("previous_") {
            prior.observe(&context.request_id, row.input, cached, 200);
        } else {
            smart.observe(&context.request_id, row.input, cached, 200);
        }
        let elapsed_hours = (row.at - first) as f64 / 3_600_000.0;
        if (20.0..=70.0).contains(&elapsed_hours) {
            diagnostics.push(Decision {
                decision_log: if kind == "legacy" {
                    String::new()
                } else {
                    DECISION_LOGGER.0.lock().unwrap().clone()
                },
                elapsed_hours,
                reason: reason.into(),
                selected: account,
                moved,
                sd_increase: dispersion(&accounts).0 - before_sd,
                before_used,
                candidate_headroom: candidates.iter().map(|c| c.headroom).collect(),
            });
        }
        previous.insert(row.session.clone(), account);
    }
    let (used_std, used_spread) = dispersion(&accounts);
    let (time_mean_sd, time_p90_sd) = time_metrics.metrics();
    let mut elapsed = 0u64;
    let mut timeline = time_metrics
        .segments
        .iter()
        .map(|(sd, dt)| {
            let point = Point {
                elapsed_hours: elapsed as f64 / 3_600_000.0,
                used_std: *sd,
            };
            elapsed += dt;
            point
        })
        .collect::<Vec<_>>();
    timeline.push(Point {
        elapsed_hours: elapsed as f64 / 3_600_000.0,
        used_std,
    });
    diagnostics.sort_by(|a, b| b.sd_increase.total_cmp(&a.sd_increase));
    diagnostics.truncate(10);
    ResultRow {
        strategy: kind.into(),
        lambda: matches!(kind, "full" | "previous_full").then_some(lambda),
        effective_tokens,
        used_std,
        used_spread,
        time_mean_sd,
        time_p90_sd,
        diagnostics,
        migrations,
        reserve_hours: accounts
            .iter()
            .map(|a| a.reserve_ms as f64 / 3_600_000.0)
            .sum(),
        all_gated_requests: all_gated,
        timeline,
        quota_events,
        calibration_snapshots,
    }
}
fn seeded_models(base: &[Model], scenario: &str, seed: u64) -> Vec<Model> {
    let mut models = base.to_vec();
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    match scenario {
        "unequal_start" => {
            for (m, used) in models.iter_mut().zip([10.0, 35.0, 60.0, 80.0]) {
                m.initial_weekly_used = used;
                m.weekly_capacity = 120_000_000.0;
            }
        }
        "staggered_reset" => {
            for (m, hours) in models.iter_mut().zip([36, 72, 120, 168]) {
                m.weekly_reset_seconds = hours * 3600;
            }
        }
        "heterogeneous" => {
            for (i, m) in models.iter_mut().enumerate() {
                let weight = if i == 0 { 20.0 } else { 1.0 };
                m.plan_prior = weight;
                m.weekly_capacity = 240_000_000.0 * weight / 23.0;
                m.five_hour_capacity = Some(20_000_000.0 * weight / 23.0);
            }
        }
        _ => {}
    }
    for i in (1..models.len()).rev() {
        models.swap(i, (next() as usize) % (i + 1));
    }
    // A common random 5h reset phase tests burst timing without inventing usage.
    let phase = 3600 + next() % 14401;
    for m in &mut models {
        m.five_hour_reset_seconds = phase;
    }
    models
}
fn external_events(first: u64, last: u64, count: usize, seed: u64) -> Vec<ExternalEvent> {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    let duration = last.saturating_sub(first);
    let mut events = (0..1 + seed % 2)
        .map(|i| ExternalEvent {
            at: first + duration / 5 + next() % (duration / 2).max(1),
            account: (seed as usize + i as usize) % count,
            weekly_points: 10.0 + (next() % 2001) as f64 / 100.0,
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|e| e.at);
    events
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    log::set_logger(&DECISION_LOGGER).map_err(|_| "decision logger already installed")?;
    log::set_max_level(log::LevelFilter::Info);
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
    let models: Vec<Model> = if let Some(config) = args.get(2).filter(|s| s.as_str() != "--suite") {
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
        simulate(&rows, &models, "previous_placement", 0.0),
        simulate(&rows, &models, "previous_full", 0.1),
        simulate(&rows, &models, "review08", 0.1),
    ];
    for lambda in [0.05, 0.1, 0.2, 0.5, 1.0] {
        results.push(simulate(&rows, &models, "full", lambda));
    }
    let mut suite = Vec::new();
    if args.iter().any(|a| a == "--suite") {
        for scenario in [
            "equal",
            "unequal_start",
            "staggered_reset",
            "heterogeneous",
            "external_burst",
        ] {
            for seed in [17, 29, 43, 59, 71] {
                if scenario != "unequal_start" && seed > 43 {
                    continue;
                }
                let models = seeded_models(&models, scenario, seed);
                let events = if scenario == "external_burst" {
                    external_events(rows[0].at, rows.last().unwrap().at, models.len(), seed)
                } else {
                    vec![]
                };
                let mut comparisons = vec![
                    simulate_events(&rows, &models, "legacy", 0.0, &events),
                    simulate_events(&rows, &models, "review08", 0.1, &events),
                ];
                for lambda in [0.05, 0.1, 0.2, 0.5, 1.0] {
                    comparisons.push(simulate_events(&rows, &models, "full", lambda, &events));
                }
                suite.push(serde_json::json!({"scenario":scenario,"seed":seed,"models":models,"external_events":events.iter().map(|e| serde_json::json!({"hour":(e.at-rows[0].at) as f64/3_600_000.0,"account":e.account,"weekly_points":e.weekly_points})).collect::<Vec<_>>(),"results":comparisons}));
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"requests":rows.len(),"default_lambda":router::DEFAULT_LAMBDA,"same_account_within_30s":short,
        "low_reuse_within_30s":low,"low_reuse_percent":100.0*low as f64/(short.max(1) as f64),"results":results,"suite":suite})
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
    fn time_dispersion_weights_duration_and_p90_not_request_count() {
        let mut m = TimeDispersion::default();
        m.add(10.0, 1);
        m.add(1.0, 99);
        assert!((m.metrics().0 - 1.09).abs() < 1e-9);
        assert_eq!(m.metrics().1, 1.0);
    }
    #[test]
    fn time_dispersion_integrates_resets_inside_idle_intervals() {
        let mut a = model(1e6);
        a.initial_weekly_used = 20.0;
        a.weekly_reset_seconds = 1;
        let mut accounts = vec![Account::new(a, 1), Account::new(model(1e6), 1)];
        let mut m = TimeDispersion::default();
        advance_accounts(&mut accounts, 1, 2001, &mut m);
        assert!((m.metrics().0 - 100.0 / 19.0).abs() < 1e-9);
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
    fn external_burst_changes_quota_but_not_pool_billing() {
        let rows = [
            Row {
                at: 1,
                session: "main:s".into(),
                model: "sol".into(),
                input: 100,
                cached: 0,
                original_account: "a".into(),
            },
            Row {
                at: 3_600_001,
                session: "main:s".into(),
                model: "sol".into(),
                input: 100,
                cached: 0,
                original_account: "a".into(),
            },
        ];
        let mut a = model(1_000_000.0);
        a.five_hour_capacity = Some(100_000.0);
        let events = [ExternalEvent {
            at: 1_800_001,
            account: 0,
            weekly_points: 20.0,
        }];
        for kind in ["legacy", "review08", "full"] {
            let result = simulate_events(&rows, &[a.clone()], kind, 0.2, &events);
            assert_eq!(result.effective_tokens, 200.0);
            assert!((result.reserve_hours - 0.5).abs() < 1e-9);
            assert!(result.time_mean_sd.abs() < 1e-9);
            assert_eq!(result.quota_events[0]["event"], "external_burst");
        }
    }
    #[test]
    fn external_bursts_are_deterministic_and_inside_trace() {
        for seed in [17, 29, 43] {
            let events = external_events(1, 604_800_001, 4, seed);
            assert_eq!(events.len(), 2);
            assert_eq!(
                serde_json::to_value(&events).unwrap(),
                serde_json::to_value(external_events(1, 604_800_001, 4, seed)).unwrap()
            );
            assert!(events.iter().all(|e| e.at > 1
                && e.at < 604_800_001
                && (10.0..=30.0).contains(&e.weekly_points)));
            assert_ne!(events[0].account, events[1].account);
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
