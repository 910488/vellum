# Third-party notices — Enhanced Codex MVP ports

This crate contains substantial ports of published mechanisms. It is not a
clean-room reimplementation. Each port below is independently gated and may
be removed without affecting the others.

Official OpenAI Codex remains unmodified. These modules are intended to land
in a pinned Enhanced Codex fork (`codex-rs/core/src/enhanced/`) and are
kept here as the portable, testable source for that fork.

---

## Port A — Qwen Tool Reliability

- **Port name:** `qwen_tool_reliability`
- **Source repository:** https://github.com/QwenLM/qwen-code
- **Source commit:** `2b8f73c1e9cf8b355ec46c4623398c27b458b076`
- **Source paths:**
  - `packages/core/src/core/turn.ts`
  - `packages/core/src/core/client.ts`
  - `packages/core/src/core/llm-chat.ts`
  - `packages/cli/src/ui/hooks/use-llm-stream.ts`
- **Original license:** Apache License 2.0
- **Ported behavior:** provider tool-call identity, name + normalized-argument
  fingerprint, handled-call ledger, duplicate provider-call detection,
  synthetic duplicate/failed results, late real-result suppression,
  resume-safe dedup.
- **Not ported:** Qwen orphan-tool repair, Qwen todo system, Qwen plan mode,
  Qwen full agent loop, Qwen full compactor.
- **Modifications:** rewritten in Rust against Codex thread/session ownership;
  never emits synthetic success; same-id/different-args fails closed as a
  provider protocol collision.
- **Destination path:** `crates/vellum-enhanced-codex/src/tool_reliability.rs`
  (fork destination: `codex-rs/core/src/enhanced/tool_reliability.rs`)

Copyright of the original Qwen Code sources remains with their authors.
This port is a modified excerpt of those mechanisms.

---

## Port B — DeepSeek Context Recovery

- **Port name:** `deepseek_context_recovery`
- **Source repository:** https://github.com/deepseek-ai/deepseek-harness
- **Source commit:** `dd6322d604e00eec1ba5e0c8541159906a21094a`
- **Source paths:**
  - `packages/compaction/compaction-basic/src/index.ts`
  - `packages/compaction/compaction-tool-result-pruner/`
  - `docs/subsystems/compaction.md`
- **Original license:** MIT License
- **Ported behavior:** deterministic tool-result pruning (`head + omitted
  marker + tail`), prune → re-measure, skip Codex local compact when the
  pruned surface is already under the compact threshold, bounded context
  overflow retry that requires real surface progress.
- **Not ported:** DeepSeek Cordis, event bus, session store, tool registry,
  permission system, subagent runtime, model-specific compaction prompts.
- **Modifications:** rewritten in Rust; prunes only model-visible tool-result
  text; durable history is out of scope; overflow retry is capped at one and
  cancellation always wins.
- **Destination path:** `crates/vellum-enhanced-codex/src/context_pruner.rs`
  and `crates/vellum-enhanced-codex/src/context_recovery.rs`
  (fork destination: `codex-rs/core/src/enhanced/`)

Copyright of the original DeepSeek Harness sources remains with their authors.
This port is a modified excerpt of those mechanisms.

---

## Port C — Qwen Bounded Continuation

- **Port name:** `qwen_bounded_continuation`
- **Source repository:** https://github.com/QwenLM/qwen-code
- **Source commit:** `2b8f73c1e9cf8b355ec46c4623398c27b458b076`
- **Source path:** `docs/design/daemon-todo-stop-guard.md`
- **Original license:** Apache License 2.0
- **Ported behavior:** at most two extra primary-model auto-continuations per
  uninterrupted user-input stage; cancel and user steer have priority.
- **Not ported:** Qwen todo DB, todo tool, todo state machine, LLM-as-judge
  completion checks, natural-language stop analysis.
- **Modifications:** rewritten in Rust; unfinished work may only come from
  Codex-native deterministic state (plan/task, native subagent work, or
  structured pending work already owned by the runtime). Missing signal
  means `ALLOW_STOP`.
- **Destination path:** `crates/vellum-enhanced-codex/src/bounded_continuation.rs`
  (fork destination: `codex-rs/core/src/enhanced/bounded_continuation.rs`)

Copyright of the original Qwen Code sources remains with their authors.
This port is a modified excerpt of those mechanisms.

---

## License text — DeepSeek Harness (Port B)

```text
MIT License

Copyright (c) 2026 DeepSeek

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

Qwen Code (Ports A and C) is licensed under the Apache License 2.0
(copyright 2025 Google LLC and 2025 Qwen); its full text is the repository
`LICENSE`. Qwen Code ships no NOTICE file at the pinned commit.
