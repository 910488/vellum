# Third-party notices

Vellum bundles material from the following third-party projects.

## OpenAI Codex

- Project: <https://github.com/openai/codex>
- Version: `rust-v0.150.0-alpha.8` (commit `fcbdb57851be70192fd0c21faa9e529146e93ff1`)
- License: Apache License 2.0
- Vendored under: `third_party/codex-0.150/`

Vellum vendors the Codex compaction prompt templates verbatim and reimplements
the Codex 0.150 local-compaction algorithm against Vellum's JSON proxy runtime
as `codex_local_v0_150`. The work is **modified** relative to upstream; the full
statement of modification, the vendored file inventory, and SHA-256 digests for
every file are recorded in `third_party/codex-0.150/PROVENANCE.md`.

The Apache-2.0 license text and upstream NOTICE are reproduced at
`third_party/codex-0.150/LICENSE` and `third_party/codex-0.150/NOTICE`.

## Qwen Code and DeepSeek Harness

- Qwen Code: <https://github.com/QwenLM/qwen-code> — Apache License 2.0
- DeepSeek Harness: <https://github.com/deepseek-ai/deepseek-harness> — MIT License

`crates/vellum-enhanced-codex` contains modified Rust ports of mechanisms from
these projects. The source commit, source paths, and statement of modification
for each port are recorded in `crates/vellum-enhanced-codex/THIRD_PARTY_NOTICES.md`.
