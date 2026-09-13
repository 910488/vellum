# Enhanced Codex fork seams

Vellum portable ports live in `crates/vellum-enhanced-codex`. They are **not**
an Enhanced Codex runtime until they are compiled into a pinned OpenAI Codex
fork and called from the agent loop.

## Required fork

```text
origin: openai/codex @ the `codexUpstreamCommit` in `enhanced-runtime.lock.json`
fork:   910488/enhanced-codex-core @ `enhancedCodexCommit` in that lock
        (currently a7b9610e18f9da1cb207cd783ba7f3f467492d77)
```

Do not track `main` / `master` / `latest`.

## Apply

```powershell
pwsh -File scripts/bootstrap-enhanced-codex-fork.ps1 -CodexRoot <clone>
```

The script copies portable `*.rs` into `codex-rs/core/src/enhanced/`, emits a
slim `enhanced/mod.rs` (no isolation tests), and inserts `mod enhanced;` after
the crate inner attributes. Codex-only `runtime.rs` (JSON loader) and
`seams.rs` (agent-loop adapters) live in the fork, not the portable crate.

Modules use `super::` so they compile in both trees. Wire the lifecycle in
`SEAMS.md` (plan/commit/release, overflow reset) before building. After a
successful build, pin:

```json
"enhancedCodexCommit": "<40-hex>",
"artifactSha256": "sha256:<64-hex>",
"targetTriple": "x86_64-pc-windows-msvc"
```

Eval `matrix` with `--ablation-profile` fails closed until those fields are
real, `VELLUM_ENHANCED_CODEX` points at the hashed binary, and `run.json`
records the resolved runtime digest separately from the ablation profile.
