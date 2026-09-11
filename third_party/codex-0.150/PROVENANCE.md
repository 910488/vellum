# Vendored provenance — OpenAI Codex 0.150 compaction

| Field | Value |
| --- | --- |
| Upstream project | [openai/codex](https://github.com/openai/codex) |
| Tag | `rust-v0.150.0-alpha.8` |
| Commit | `fcbdb57851be70192fd0c21faa9e529146e93ff1` |
| Annotated tag object | `4111e744b09ceed970f5d88e7775ca583f963ec4` |
| License | Apache-2.0 (see `LICENSE`, `NOTICE`) |
| Vellum engine id | `codex_local_v0_150` |
| Vellum provenance string | `openai-codex@rust-v0.150.0-alpha.8+fcbdb578` |

The build does **not** fetch upstream. Every byte the runtime depends on is
committed under this directory.

## Vendored verbatim

These files are byte-identical copies of the upstream tag and are compiled
into the runtime via `include_str!`.

| Vendored path | Upstream path | SHA-256 |
| --- | --- | --- |
| `templates/compact/prompt.md` | `codex-rs/prompts/templates/compact/prompt.md` | `bcdd8c0240c38c88a1beb53b0736a606bed60b278e0170912d4ebbc7d3b419eb` |
| `templates/compact/summary_prefix.md` | `codex-rs/prompts/templates/compact/summary_prefix.md` | `e9b088e794a6bb9082ac053fcc760bd818d7e720ee4bcdc72c6e480de7b7cb0e` |
| `LICENSE` | `LICENSE` | `aa5e89edcbbd01fc3fb188a527d8bdc0da5812305cab220c84348c14ea427288` |
| `NOTICE` | `NOTICE` | `3c505dc54be731583470ef3584e5cb96d60df7add3e7e36294cf4dea8316a5cb` |

## Ported with modification

Not vendored as source. The compaction *semantics* below were reimplemented in
`crates/vellum-proxy-runtime/src/codex_local_v0_150.rs` against Vellum's JSON
runtime. The upstream files are recorded here so a future reviewer can diff
against the exact revision that was read.

| Upstream path | SHA-256 at this tag |
| --- | --- |
| `codex-rs/core/src/compact.rs` | `898f7263301a30132f96f2058fe1d262682045dd4cc621acdf9a6e1b9e06f049` |
| `codex-rs/prompts/src/compact.rs` | `6f90d728db32cd5e54b4f75d733d9f7a2a0ed30d5717935955817c3ab7105f2d` |

### Statement of modification (Apache-2.0 §4(b))

Vellum's port differs from upstream as follows, and these differences are
intentional:

1. **Runtime representation.** Upstream operates on typed `ResponseItem` /
   `ResponseItemEnvelope` values inside a `Session`. Vellum operates on
   `serde_json::Value` conversation items flowing through the proxy, so the
   port reimplements item classification, user-message extraction, and
   replacement-history assembly against JSON rather than reusing upstream types.
2. **No session ownership.** Upstream mutates `Session` history in place and
   emits Codex events, hooks, and analytics. The port is a set of pure
   functions plus one transport call; hooks, analytics, `WorldState`, and
   `InitialContextInjection` are not ported.
3. **Cross-provider sanitization.** Before summarization Vellum strips
   provider-private material (OpenAI ciphertext, reasoning content,
   credentials, provider metadata) that upstream never has to consider because
   it talks to one provider. This is a Vellum safety boundary, not upstream
   behavior.
4. **Not ported.** `compact_remote*` (server-side compaction),
   `compact_model_fallback`, image budgeting, and initial-context reinjection
   are outside the scope of the local-compaction port.

Everything else — the summarization prompt, the summary prefix, the
20,000-token most-recent-user-message budget with tail truncation, exclusion of
prior summary messages from that budget, use of the last assistant message as
the summary body, replacement ordering (selected user messages, then the
prefixed summary last), and the context-window retry that drops the oldest
summarizer input item one at a time — is preserved as upstream defines it.
