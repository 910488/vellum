# Harness Lab

This lab evaluates pinned upstream harness modules without rebuilding Vellum
or Enhanced Codex. It is evidence tooling, not a product execution path.

## Prepare once

```powershell
cd evals/harness-lab
pnpm bootstrap
pnpm test
```

Bootstrap clones the exact Qwen commit in `upstream-lock.json` below
`target/harness-lab/upstream`, verifies HEAD, installs its locked dependencies,
and builds only Qwen Code core. Later adapter and policy iterations require no
Rust build and no repeat upstream build.

## Replay

```powershell
pnpm replay -- --input C:\path\to\rollout.jsonl
pnpm corpus
```

The default enables Qwen's existing heuristic tier. Pass
`--upstream-default` to retain Qwen's released skip-heuristics setting while
keeping its always-on safeguards. Reports are written under
`target/harness-lab/reports`.

`pnpm corpus` resolves the pinned E1 run set in `corpus.json`, replays every
case, and reports reviewed detection coverage separately from unreviewed cases.

## Bridge

Set `VELLUM_LAB_UPSTREAM`, `VELLUM_LAB_PORT`, and `VELLUM_LAB_MODE` (`S0`,
`S1`, or `S2`), then run `pnpm bridge`. Every request must carry a unique
`x-vellum-lab-session` header when one bridge serves multiple cases. A bridge
dedicated to one case uses `VELLUM_LAB_SESSION` or its process-local default.
S0 observes, S1 stops at the next safe request boundary, and S2 injects the
fixed recovery prompt once.

The bridge exposes `/healthz` and writes anonymous detector/injection/stop
events when `VELLUM_LAB_EVENT_LOG` is set. It never logs tool arguments or
results.

The bridge buffers request JSON but streams upstream SSE unchanged. It never
retries requests. Opaque continuation inputs that are not an input array are
observed but not modified.

## Campaign state

`pnpm campaign -- --manifest campaign.selftest.json` verifies the background
runner without a model. A live manifest may add `bridge`, `artifacts`, and
commands. Each command gets its own bridge process and session; `${REPO_ROOT}`
and `${VELLUM_LAB_BASE_URL}` are expanded without a shell. State, stdout,
stderr, event paths, commit, and artifact hashes are persisted under
`target/harness-lab/campaigns`.

## Qualification boundary

Offline replay establishes detector compatibility only. S1 can establish
loss bounding, not task improvement. S2 needs paired Windows Sandbox runs and
the existing hidden Docker grader. Product promotion still requires a Rust
port, parity replay, one release build, and installed Desktop validation.

## Enhanced runtime debug smoke

`campaign.enhanced-debug-log.json` pins the Enhanced commit and binary digest,
runs one Omen and one Qwen read-only turn through the real Vellum gateway, and
writes the bounded production debug sink under
`target/enhanced-debug-smoke`. Run it with:

```powershell
pnpm campaign -- --manifest campaign.enhanced-debug-log.json
```

The commands inherit the current Codex home so the local gateway boundary
header remains outside the manifest and reports.

The smoke qualifies wiring and observability. It expects runtime identity plus
E1 tool admission/completion, E2 pressure decisions, and E3 stop decisions in
the JSONL files; it does not establish task-quality improvement.
