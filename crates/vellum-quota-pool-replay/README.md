# Quota pool replay

This offline CLI compiles the production `SmartRouter` directly from the desktop source. It includes frozen copies of the legacy router from `31c214d` and the initial PR #19 router from `e0930b7`, solely for reproducible comparisons. No desktop application is started, no credentials are opened, and SQLite is opened with read-only flags and a read transaction.

```sh
cargo build -p vellum-quota-pool-replay
cargo run -p vellum-quota-pool-replay -- 'file:/path/to/usage.sqlite3?mode=ro'
```

An optional second argument is a JSON array of synthetic accounts. Each object contains `weekly_capacity`, optional `five_hour_capacity`, `initial_weekly_used`, `initial_five_hour_used`, `weekly_floor`, `weekly_reset_seconds`, `five_hour_reset_seconds`, and `plan_prior`. Capacities are effective input tokens per whole window; reset offsets are relative to the first replayed request. The default is four equal Pro-prior accounts: 60M weekly tokens, 5M five-hour tokens, 20% initial weekly use, 5% weekly floor, and full initial reset intervals. No real account plans or quotas are inferred from credential files.

The CLI reads the latest seven days of successful Official requests with a conversation identity and nonzero input. Every comparison uses the same collected sequence. Same-account requests preserve recorded cached tokens; a migrated request is fully uncached. Output tokens are excluded because they do not change account choice. The legacy comparison retains its model-specific session key; the new comparisons retain one account affinity across models in the same route/thread.

SQLite does not retain request-side prompt estimates or context-window IDs. Replay caps the initial prompt estimate at 4,000 tokens and treats a greater-than-50% same-model input reduction as a compaction boundary. These are simulation assumptions, not reconstructed provider state. A real continuation migration must pass the runtime's account-realm check and hydrate portable history without sending an old account's `previous_response_id`; runtime and existing WebSocket integration tests cover this.

Primary balance metrics are wall-time-weighted mean normalized weekly used SD and wall-time-weighted p90 SD. SD is integrated exactly between request events and quota-reset boundaries, including idle gaps; request frequency does not weight the metric. The p90 is the inverse cumulative wall-time distribution. Final SD/spread are references only. Other metrics include effective input, migrations, summed account-hours inside the 5h reserve and all-gated fallback requests. Curves show exact event/reset steps. All-gated scenarios should not select lambda: fallback to the least-used synthetic account can artificially flatten final dispersion. The tool separately counts original same-account, same-model requests within 30 seconds whose cached input is less than half the previous input. These pre-existing low-reuse events remain in the cost ledger and are not attributed to migration.

For aggregate plots (Python with matplotlib):

```sh
python crates/vellum-quota-pool-replay/plot_results.py --replay-exe target/debug/vellum-quota-pool-replay --database 'file:/path/to/usage.sqlite3?mode=ro' --output /tmp/quota-replay-report
```

When `CARGO_TARGET_DIR` is set, point `--replay-exe` at that directory's binary. Windows adds `.exe`. Keep the JSON output local; only the aggregate comparison table and charts belong in the PR. No raw request rows, identities, account hashes, prompts, or credential material are exported.

## Decision units and bootstrap

Weekly fair rate starts with spendable tokens divided by time until reset. Add `C * (headroom_points - mean_headroom_points) / min(reset_time, 6h)` to recover accumulated percentage-point skew within hours. The mean uses eligible accounts, and `C` converts each account's correction to effective tokens/second. Non-positive budgets become a small positive numerical floor.

Every account with a 5h window receives a rate cap even before calibration: `C5 = C / 12` is the logged bootstrap ratio, replaced by independent capacity measurements from the same effective-token observations. Its cap is spendable five-hour tokens divided by time to reset, tightened further for above-average five-hour use over `min(reset_time, 1h)`. This correction never raises the base five-hour cap. The weekly and five-hour budgets both apply; the existing admission reserve remains. Missing reset times use a full window and stale reset times use a 60-second lower bound.

Account and session effective-input rates use exponential decay with a 20-minute half-life. A new session immediately reserves its learned class median, and both its reservation and observations decay; the observation clock is the bound request's admission time so replay and production agree. Main and auto-review medians retain at most 128 distinct recently observed sessions each, avoiding request-frequency weighting. Cold-start forecasts are 25 effective tokens/second for main and 5 for review; cold-start capacity is 10,000 effective tokens per weekly point times the relative plan prior (Plus 1, Pro Lite 5, Pro 20). These are explicitly logged priors, replaced by measurements rather than claims about an actual account allowance.

Each request compares the reduction in above-fair-rate load over the session's Lindy horizon (age, capped at an hour) against `0.9 * last_input * cache_heat(gap)`, with a 25% margin and 30-minute migration hysteresis. Compression removes the estimated replay penalty. Admission gates immediately select an eligible account, while a near-limit account can leave at a cold or compressed boundary. Cache warmth constants come from the supplied local seven-day Official study on 2026-10-12: up to 10 minutes 0.95, 10–30 minutes 0.5, 30–60 minutes 0.3, beyond an hour 0.05. The observed median reuse was 0.97–1.00 under five minutes, 0.66 at 10–15 minutes, 0.40 at 15–30 minutes, and zero beyond an hour; 33 observed account changes had zero reuse.

Default lambda remains 0.1. Evaluate it against whole-trace metrics and all merge thresholds, not final SD. The revision's PR reports failed thresholds prominently; it is not a recommendation to merge without accepting the documented tradeoffs.

## Scenario suite and diagnosis

Pass `--suite` in place of the model path to replay four families on the same in-memory request sequence. Each family uses deterministic seeds 17, 29 and 43 to shuffle account assignments and choose a common initial 5h reset phase between one and five hours. Equal uses the default model. Unequal-start uses 10/35/60/80% and 120M weekly capacity per account to avoid forcing all-gated fallback solely from the initial spend. Staggered-reset uses 36/72/120/168 hours until initial weekly reset. Heterogeneous has one Pro prior and three Plus priors, with capacities proportional to 20:1:1:1 and the same 240M weekly / 20M five-hour total budget as equal. These are synthetic scenarios, not actual plan allowances. A quota reset after the initial offset renews a full 7d/5h window.

```sh
cargo run -p vellum-quota-pool-replay -- 'file:/path/to/usage.sqlite3?mode=ro' --suite
python crates/vellum-quota-pool-replay/plot_results.py --input-json /tmp/quota-results.json --output /tmp/quota-plots
```

The report uses mean ± sample SD across three seeds, plus each seed's threshold result. Required limits are mean SD no worse than legacy, reserve hours no greater, and effective input no more than +0.5%. Final SD must not select the default. Low-reuse short-gap pairs remain separate.

Diagnosis retains only the top ten 20–70h decisions with the largest immediate SD increases, for each comparison: relative elapsed hour, synthetic account position, reason, before-use percentages, SD delta and the router's numeric decision log. No real account/conversation identity or absolute request time is exported. This is an immediate contribution ranking, not a causal attribution of a long session's entire future consumption. The frozen initial router exposes weak correction, prior/measured capacity asymmetry and cache-cost blocks; the revised router's logged capacities show whether 5h uses prior or measured data.
