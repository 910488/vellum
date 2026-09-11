# Enhanced MVP live manifests

Each suite now carries `ablationProfile`. `--ablation-profile` must match it
(or can be omitted, in which case the suite value is used). The runner writes
`$CODEX_HOME/enhanced-runtime.json` so Enhanced Codex can actually apply E0–E5.
That file is ignored by unmodified Official Codex.

```text
cargo run -p vellum-eval --bin vellum-eval -- matrix `
  --suite evals/manifests/enhanced-mvp-e0.json `
  --ablation-profile E0 `
  --compaction-engine codex-local `
  --repeat 1
```

Use `enhanced-mvp-e1.json` … `enhanced-mvp-e5.json` as report labels for the
matching `--ablation-profile`. Do not infer the profile from a model name.
Exact model ids must appear in the report. Promotion still requires
`legacy_harness_mutation_count == 0` and the hard safety gates in
`docs/enhanced-runtime-mvp.md`.
