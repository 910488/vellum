# Vellum documentation

This directory is the entry point for current Vellum engineering documents.
Use the authority order below when documents overlap.

## Authority order

1. `AGENTS.md` — repository-wide execution rules for Codex and contributors.
2. `protocol-source-of-truth.md` — normative model proxy protocol contract.
3. `git-worktree-branch-policy.md` — normative Git/worktree/release-branch policy.
4. Active ADRs under `adr/` — narrow architectural decisions not superseded
   by the sources above.
5. Operational and acceptance documents — current procedures only; they do
   not override the normative contracts.

## Normative documents

- [Proxy protocol source of truth](protocol-source-of-truth.md)
- [Git worktree and branch policy](git-worktree-branch-policy.md)
- [Build and release instructions](../BUILDING.md)

## Public release

- [Public release and history-sanitization checklist](public-release-checklist.md)

## Standard QA

- [Standard test process](qa-standard-process.md)
- [Acceptance matrix (v1)](qa-acceptance-matrix.md)
- [Windows Sandbox QA lane](qa-sandbox.md)

These are operational. They do not override the protocol contract. `pnpm qa -- --lane offline` is the free CI lane; desktop / remote / live are operator-started.

## Remote Manager

- [Current architecture and operations](remote-proxy-manager.md)
- [Acceptance matrix](remote-acceptance.md)
- [Clean-host acceptance](remote-clean-host-acceptance.md)

Acceptance documents define reproducible commands and pass criteria. Store run
artifacts outside the repository; an older result is not a current qualification.

## Active ADRs

- [ADR-001: Codex stays host-native](adr/ADR-001-codex-host-native.md)
- [ADR-002: Proxy may be containerized](adr/ADR-002-proxy-containerized.md)
- [ADR-005: Managed Profile is default](adr/ADR-005-managed-profile-default.md)
- [ADR-006: Adopt Existing uses three-way lease restoration](adr/ADR-006-three-way-lease.md)
- [ADR-007: Proxy must be ready before injection](adr/ADR-007-proxy-health-before-injection.md)
- [ADR-008: Mutations are idempotent operations](adr/ADR-008-idempotent-mutations.md)
- [ADR-009: Native Harness Manager](adr/ADR-009-native-harness-manager.md)

The Codex native app-server daemon is the sole thread/session authority. The
Broker is diagnostic-only and is not in the production data path.

## Evaluation

- [Enhanced harness improvement and qualification plan](enhanced-harness-qualification.md)
- [Enhanced harness fast iteration gate](enhanced-harness-fast-iteration.md)

Dated eval reports and qualification runs are kept with their run artifacts,
outside the repository. They are evidence for the recorded commit and sample
only; they do not override the protocol contract or establish promotion from a
single run.

## Harness Manager

Native-harness work is governed by [ADR-009](adr/ADR-009-native-harness-manager.md)
and [Harness Manager](harness-manager.md).
It is an incremental architecture: the proxy protocol source of truth remains
the authority for the existing `vellum-generic` data plane.

Built-in ZCode Start Plan / GLM cannot be driven by a standalone app-server
that fakes runtime headers. Attach is
[ZCode Desktop host stdio tap](zcode-desktop-host-tap.md)
(`vellum-zcode-desktop` control channel). Desktop UI promotion still needs
a per-release `cjsSha256` pin.

## Enhanced Codex Runtime MVP

Third-party Desktop threads bind to a pinned Enhanced Codex execution plane.
See [Enhanced Codex Runtime MVP](enhanced-runtime-mvp.md). Official OpenAI /
GPT traffic stays on unmodified Codex. Do not fold these ports into
`vellum-proxy-runtime`.

## Maintenance rules

- Update a normative document in the same commit as the behavior it governs.
- Keep dated qualification output and environment-specific evidence in
  external run artifacts, not source documentation.
- Remove a superseded ADR after its replacement is identified in this index;
  Git history remains the archive.
- Do not create branch-specific architecture documents. Platform-specific
  behavior lands through `main`, and `MAC_OS` mirrors `main`.
