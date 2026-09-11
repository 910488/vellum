# ADR-005: Managed Profile is default

## Status

Accepted; reviewed 2026-08-13.

## Context

Changing a user's production Codex home is higher risk than operating an
isolated Vellum-managed profile.

## Decision

New Remote Manager connections default to an isolated managed Codex home under
the Vellum remote data directory. Adopting an existing Codex home requires an
explicit user action and a managed-field lease.

## Consequences

- Normal bootstrap does not mutate the user's primary Codex profile.
- Restore can remove Vellum routing without deleting Codex, tasks, or user
  data.
- Adopt Existing must follow ADR-006 and fail closed on managed-field
  conflicts.

See `../remote-proxy-manager.md`.
