# ADR-008: Mutations are idempotent operations

## Status

Accepted; reviewed 2026-08-13.

## Context

SSH or Desktop disconnections can make request/result delivery uncertain. A
retry must reconcile observed state without repeating destructive effects.

## Decision

Lifecycle mutations use operation identities, are idempotent where their
contract allows retry, and return observed desired-state results. Destructive
operations act only on verified Vellum-owned resources.

## Consequences

- Install, start, stop, restore, and repair can reconcile interrupted work.
- Unknown processes, profiles, and unlabeled/mismatched containers are never
  removed as collateral cleanup.
- A command response cannot claim success when post-operation verification
  fails.

See `../remote-proxy-manager.md` and `../remote-acceptance.md`.
