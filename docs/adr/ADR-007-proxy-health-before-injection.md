# ADR-007: Proxy must be ready before injection

## Status

Accepted; reviewed 2026-08-13.

## Context

Injecting Codex routing before the proxy is usable leaves Codex pointed at a
dead, stale, or wrong runtime.

## Decision

The required deployment order is: prepare/start proxy, verify `/readyz`,
verify install/config identity and the Responses HTTP/WebSocket contract,
validate catalog, acquire the lease, apply managed fields, verify, then start
or restart the managed native Codex daemon when required.

## Consequences

- The Agent refuses injection when readiness or identity does not match the
  desired deployment.
- A partially started proxy is rolled back instead of being recorded as
  healthy.
- UI success requires observed state, not only a completed command.

See `../remote-clean-host-acceptance.md`.
