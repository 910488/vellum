# ADR-006: Adopt Existing uses three-way lease restoration

## Status

Accepted; reviewed 2026-08-13.

## Context

A user or another tool may edit Codex configuration after Vellum applies its
managed fields. Blind restoration could destroy those later edits.

## Decision

Restore compares Original, Applied-by-Vellum, and Current-on-disk values. It
rewrites a managed field only when the current value still equals the value
Vellum applied.

## Consequences

- Unrelated and later user edits survive restoration.
- A conflicting managed-field edit fails closed as a configuration conflict.
- A lease must identify the exact profile and Vellum-applied values.

See `../remote-proxy-manager.md` and `../remote-acceptance.md`.
