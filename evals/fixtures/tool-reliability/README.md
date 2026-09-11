# Tool reliability fixtures

Mechanism evidence is the in-process Port A runner:

```text
cargo test -p vellum-enhanced-codex --lib tool_reliability
cargo run -p vellum-eval --bin vellum-eval -- enhanced-mvp-fixtures
```

Those inject duplicate provider call IDs, fingerprint collisions, late
results, and resume. `solution.py` / `unique_sorted` is only a live coding
wrapper and is **not** a tool-reliability hard gate.
