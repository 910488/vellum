# Jetson Isolated Live Smoke

## Safety model

Shared with production Codex:

- native binary only:
  `/home/vellum-test/.local/lib/node_modules/@openai/codex/node_modules/@openai/codex-linux-arm64/vendor/aarch64-unknown-linux-musl/bin/codex`
- a *copy* of `auth.json` / `config.toml`

Never used:

- `/home/vellum-test/.codex` as `CODEX_HOME`
- `/home/vellum-test/.codex/app-server-control/app-server-control.sock`
- `/home/vellum-test/.codex/app-server-control/desktop-ssh-websocket-v0.sock`
- production sessions / state DB / logs DB / app-server daemon locks

Each run:

```text
/home/vellum-test/.local/state/vellum-smoke/<RUN_ID>/
/run/user/1000/vellum-smoke/<SHORT_ID>/app.sock
systemd user units: vellum-codex-smoke-<SHORT_ID>.service
                    vellum-broker-smoke-<SHORT_ID>.service
```

## Prerequisites

1. SSH to Jetson works with BatchMode:
   `ssh tester@192.0.2.10 true`
2. Cross-build broker for aarch64 Linux (Jetson has no rustc):

```powershell
rustup target add aarch64-unknown-linux-gnu
# or provide an existing binary via VELLUM_LIVE_BROKER_BIN
```

3. Env:

```powershell
$env:VELLUM_LIVE_SMOKE="YES"
$env:VELLUM_LIVE_SSH="tester@192.0.2.10"
$env:VELLUM_LIVE_BROKER_BIN="target/aarch64-unknown-linux-gnu/debug/vellum-remote-broker"
# pin allowlist to installed Jetson version
$env:VELLUM_LIVE_ALLOWED_VERSIONS="0.146.0"
```

## Run

```powershell
cargo test -p vellum-remote-testkit --features live-smoke --test live_jetson -- --ignored --nocapture --test-threads=1
```

Single test:

```powershell
cargo test -p vellum-remote-testkit --features live-smoke --test live_jetson jetson_live_ready -- --ignored --nocapture
```

Artifacts:

```text
target/live-smoke/<RUN_ID>/summary.json
target/live-smoke/<RUN_ID>/app-server.log
target/live-smoke/<RUN_ID>/broker.log
```
