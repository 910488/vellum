# Quota pool replay

This offline CLI compiles the production `SmartRouter` directly from the desktop source. It also includes a frozen copy of the pre-change router from `31c214d`, solely for comparison. No desktop application is started, no credentials are opened, and SQLite is opened with read-only flags and a read transaction.

```sh
cargo build -p vellum-quota-pool-replay
cargo run -p vellum-quota-pool-replay -- 'file:/path/to/usage.sqlite3?mode=ro'
```

An optional second argument is a JSON array of synthetic accounts. Each object contains `weekly_capacity`, optional `five_hour_capacity`, `initial_weekly_used`, `initial_five_hour_used`, `weekly_floor`, `weekly_reset_seconds`, `five_hour_reset_seconds`, and `plan_prior`. Capacities are effective input tokens per whole window; reset offsets are relative to the first replayed request. The default is four equal Pro-prior accounts: 60M weekly tokens, 5M five-hour tokens, 20% initial weekly use, 5% weekly floor, and full initial reset intervals. No real account plans or quotas are inferred from credential files.

The CLI reads the latest seven days of successful Official requests with a conversation identity and nonzero input. Every comparison uses the same collected sequence. Same-account requests preserve recorded cached tokens; a migrated request is fully uncached. Output tokens are excluded because they do not change account choice. The legacy comparison retains its model-specific session key; the new comparisons retain one account affinity across models in the same route/thread.

SQLite does not retain request-side prompt estimates or context-window IDs. Replay caps the initial prompt estimate at 4,000 tokens and treats a greater-than-50% same-model input reduction as a compaction boundary. These are simulation assumptions, not reconstructed provider state. A real continuation migration must pass the runtime's account-realm check and hydrate portable history without sending an old account's `previous_response_id`; runtime and existing WebSocket integration tests cover this.

Metrics include total effective input, normalized weekly used standard deviation and maximum spread (percentage points), account changes, summed account-hours inside the 5-hour reserve, all-gated fallback requests, and hourly dispersion. All-gated scenarios should not select lambda: fallback to the least-used synthetic account can artificially flatten final dispersion. The tool separately counts original same-account, same-model requests within 30 seconds whose cached input is less than half the previous input. These pre-existing low-reuse events remain in the cost ledger and are not attributed to migration.

For aggregate plots (Python with matplotlib):

```sh
python crates/vellum-quota-pool-replay/plot_results.py --replay-exe target/debug/vellum-quota-pool-replay --database 'file:/path/to/usage.sqlite3?mode=ro' --output /tmp/quota-replay-report
```

When `CARGO_TARGET_DIR` is set, point `--replay-exe` at that directory's binary. Windows adds `.exe`. Keep the JSON output local; only the aggregate comparison table and charts belong in the PR. No raw request rows, identities, account hashes, prompts, or credential material are exported.

## Decision units and bootstrap

Weekly fair rate is spendable percentage points times calibrated tokens per point, divided by time until reset. A measured five-hour rate adds another cap; without its capacity measurement, the existing 5% admission reserve and a penalty over the last 15 spendable points remain. Missing reset times use a full window, and stale reset times use a 60-second lower bound.

Account and session effective-input rates use exponential decay with a 20-minute half-life. A new session immediately reserves its learned class median, and both its reservation and observations decay; the observation clock is the bound request's admission time so replay and production agree. Main and auto-review medians retain at most 128 distinct recently observed sessions each, avoiding request-frequency weighting. Cold-start forecasts are 25 effective tokens/second for main and 5 for review; cold-start capacity is 10,000 effective tokens per weekly point times the relative plan prior (Plus 1, Pro Lite 5, Pro 20). These are explicitly logged priors, replaced by measurements rather than claims about an actual account allowance.

Each request compares the reduction in above-fair-rate load over the session's Lindy horizon (age, capped at an hour) against `0.9 * last_input * cache_heat(gap)`, with a 25% margin and 30-minute migration hysteresis. Compression removes the estimated replay penalty. Admission gates immediately select an eligible account, while a near-limit account can leave at a cold or compressed boundary. Cache warmth constants come from the supplied local seven-day Official study on 2026-10-12: up to 10 minutes 0.95, 10–30 minutes 0.5, 30–60 minutes 0.3, beyond an hour 0.05. The observed median reuse was 0.97–1.00 under five minutes, 0.66 at 10–15 minutes, 0.40 at 15–30 minutes, and zero beyond an hour; 33 observed account changes had zero reuse.

Default lambda is 0.1, selected from the sampled Pareto frontier in the capacity-sufficient 60M/5M model. The PR includes all five lambdas, the placement-only alternative, and capacity sensitivity: this is an empirical default, not a universal optimum. The five-hour reserve metric can trade off against cache cost and weekly balance; no improvement on every metric is asserted. Lambda is not a UI setting.
