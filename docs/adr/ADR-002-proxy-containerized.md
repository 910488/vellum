# ADR-002: Proxy may be containerized

## Status

Accepted; reviewed 2026-08-13.

## Context

The model data plane needs an independent lifecycle, multi-architecture
packaging, and isolation from Codex state.

## Decision

`vellum-proxy-daemon` may run in Docker with a loopback-only host binding,
read-only configuration/credential mounts, and a source-fingerprinted image.
The bundled image is built from the shared `vellum-proxy-runtime` used by the
Desktop bridge.

## Consequences

- The proxy container receives no Docker socket, Codex home, or workspace
  mount.
- The Agent manages containers by Vellum ownership labels and verified
  install/config identity, never by name alone.
- A proxy-source change requires a fresh embedded image build and Remote
  Manager re-sync.

See `../protocol-source-of-truth.md` and `../../BUILDING.md`.
