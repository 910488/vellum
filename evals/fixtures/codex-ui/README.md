# Codex UI fixtures

`bootstrap-jsonrpc.jsonl` is a hand-written *method-surface* fixture, not a
capture. Every method in it is verified against the pinned schema by
`vellum-codex-facade/tests/schema_conformance.rs`, so the names are real, but
the ordering and parameter completeness are Vellum's assumption about how a
client drives a session — not observed Codex TUI behaviour.

`harnessSelection` on `thread/start` is a Vellum extension. Real Codex does not
send it; the facade requires it to know which harness owns the thread.

## Still outstanding (plan §28, §60)

A real recording of the pinned Codex client's bootstrap traffic has not been
taken. Until it is, the facade's method surface is inferred from the schema
rather than from what the client actually sends, and a required bootstrap call
Vellum does not implement would only show up when a real client connects.

The seam for that capture exists: `codex app-server proxy --sock <PATH>` speaks
the app-server protocol to a control socket, so a recorder that owns the socket
can log a real session's traffic.
