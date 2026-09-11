# Generated Codex app-server schema

This directory should contain schema artifacts generated from the pinned Codex version:

```bash
codex app-server generate-json-schema --out crates/vellum-remote-broker/src/app_server/generated/schema
codex app-server generate-ts --out crates/vellum-remote-broker/src/app_server/generated/ts
```

`VERSION` records the pinned Codex CLI/app-server version used for generation.

CI should regenerate schema into a temp directory and fail on unexpected diffs.
