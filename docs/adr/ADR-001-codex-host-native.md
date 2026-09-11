# ADR-001: Codex stays host-native

## Status

Accepted; reviewed 2026-08-13.

## Context

Remote Vellum must preserve the Codex workspace, shell, sandbox, approvals,
thread identity, and native app-server semantics. Containerizing Codex would
change those host-owned contracts and create a second session authority.

## Decision

Codex runs as the host-native Codex app-server daemon and remains the sole
authority for threads, turns, approvals, workspaces, and sessions. The Remote
Agent manages its lifecycle and queries its native control API. The Broker is
legacy/diagnostic-only and is not part of the production session data path.

## Consequences

- The model proxy may be isolated without containerizing Codex.
- Remote Manager must not synthesize a second durable session store.
- Detach/resume validation uses the native thread ID and control API.

See `../remote-proxy-manager.md` and `../remote-acceptance.md`.
