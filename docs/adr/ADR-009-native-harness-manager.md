# ADR-009: Native Harness Manager

## Status

Accepted for incremental implementation.

## Decision

Each Vellum UI thread is bound to exactly one selected harness runtime. That
runtime is authoritative for its native session, agent loop, tool policy,
compaction, memory, subagents, and provider inference. Vellum provides
discovery, lifecycle management, session binding, capability negotiation,
event normalization, permission mediation, audit, and UI protocol adaptation.

The event journal is explicitly an audit/reconnect mirror, never a transcript
replay source. Unsupported native capability remains unsupported; it must not
fall back to a second harness.

The existing proxy runtime remains the `vellum-generic` harness path for
models without an official native coding harness. Its existing protocol
invariants remain authoritative for that path.

## Consequences

The implementation begins with pure harness protocol types, shared ACP stdio
transport, process supervision, deterministic fake ACP tests, and a Grok
adapter. A Codex App Server facade may only be enabled after the bundled,
pinned protocol bootstrap traffic has been captured and replay-tested.
