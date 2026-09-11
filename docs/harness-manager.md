# Harness Manager

ZCode's built-in Start Plan / GLM still requires the signed-in Desktop host
for CAPTCHA-bound runtime headers. Attach is the stdio spawn override plus
the `vellum-zcode-desktop` control channel in
[ZCode Desktop host stdio tap](zcode-desktop-host-tap.md). It intercepts
Desktop's app-server child; it does not forge `headersApplied`. Do not
acknowledge `interaction/requestProviderRuntimeHeaders` with a fabricated
`headersApplied: true` on an independent app-server.

## Runtime authority

Every UI thread is bound once to one selected native harness. The binding store
is only an index; it is never a transcript or recovery source. A missing native
session therefore returns `SessionNotFound`/unsupported resume instead of
replaying UI history into another runtime.

## Codex facade compatibility

The facade protocol pin lives in `third_party/codex-app-server-schema/0.142.5/`,
generated from the installed Codex with:

```text
codex app-server generate-json-schema --out third_party/codex-app-server-schema/<version>
```

`methods.rs` names every method, notification, server request, thread-item type
and status the facade uses, and `tests/schema_conformance.rs` asserts each one
against that generated schema. The facade therefore cannot invent protocol: a
name Codex does not define fails the test rather than reaching a UI that will
never send or understand it.

Release qualification regenerates the schema, compares its SHA-256, and replays
`evals/fixtures/codex-ui/bootstrap-jsonrpc.jsonl`. A changed hash requires an
explicit facade review; it must not silently widen the method surface.

### What the pin corrected

The first draft of the facade guessed its method names. Generating the schema
showed three of them were not Codex protocol at all:

- `thread/settings/update` does not exist. Model and reasoning effort are
  written with `config/value/write`.
- `permission/resolve` does not exist. Approval is a **server request**
  (`item/permissions/requestApproval`) that the UI answers with a JSON-RPC
  response, which is why the facade has `handle_response` alongside
  `handle_request`.
- `plan/*` and `terminal/*` do not exist. The real command surface is
  `command/exec`, and plans arrive as `turn/plan/updated`.

This is what §28 means by "do not guess".

## ACP runtime adapters

Grok Build (`grok agent stdio --no-leader`), Qwen Code (`qwen --acp`), and
DeepSeek Harness (`dsh --profile acp`) all use one bounded JSON-RPC stdio
client. Stdout is protocol-only and stderr is excluded from the protocol.
Native notifications are filtered by native session ID before they are exposed
to an UI thread.

DeepSeek's descriptor deliberately reports plans, commands, terminals,
subagents, and compaction as unsupported. The facade must fail closed instead
of emulating these capabilities.

## Two-stage event mapping

Native events never become UI events in one step:

```text
native protocol
  -> vellum-harness-runtime::mapper::NativeEventMapper  -> HarnessEvent
  -> vellum-codex-facade::ui_mapper::CodexUiEventMapper -> Codex notification
```

`AcpEventMapper` covers the ACP surface shared by Grok Build, Qwen Code and
DeepSeek Harness. Anything it does not recognise is preserved verbatim as a
`NativeExtension` event under a per-vendor namespace (`xai`, `qwen`,
`deepseek`) and reaches the UI as `thread/nativeEvent`. The UI may ignore such
an event; Vellum must not discard it. Replacing the UI means replacing the
second stage only.

## Permission mediation

Permissions are correlated by identifier, never by matching text:

```text
native session/request_permission (JSON-RPC request id retained by the adapter)
  -> PermissionRequested event carrying the native request id
  -> PermissionRegistry issues a Vellum `perm_*` id
  -> item/permissions/requestApproval sent to the UI as a server request
  -> UI replies with a JSON-RPC response (a granted permission profile)
  -> CodexAppServerFacade::handle_response consumes the binding
  -> adapter answers the original native JSON-RPC id
```

A response carrying no granted profile is a refusal. Absence of a grant is
never read as an implicit yes.

The binding is consumed by its first resolution and the adapter drops its
retained request id at the same time, so a duplicated or replayed approval
fails closed at both layers rather than reaching the native harness twice. The
native request id is never exposed to the UI.

## Event journal

`EventJournal` is `NOT_RUNTIME_REPLAY_SOURCE`. It stores *neutral* events, not
Codex notifications, because several native events have no Codex representation
at all — provider extensions, mid-flight tool updates. Keeping the neutral form
means the audit and evaluation record stays complete even where the UI shows
nothing, which is what §7 requires: the UI need not display a native event, but
Vellum must not lose it.

`thread/read` derives its Codex view from the journal on demand and tags the
response `"source": "vellumJournalMirror"`, so no consumer can mistake it for
the transcript the native harness reasons over. Replayed history never re-issues
an answerable permission request. `audit_journal()` exposes the full neutral
record for evaluation. The journal is bounded per thread.

## Capability truthfulness

`command/exec` and `thread/fork` are routed through the bound session's
capability flags. A harness that lacks the capability gets
`UnsupportedCapability`, never a plausible empty success. No harness declares
`fork`: the neutral protocol has no fork, and replaying a transcript to fake one
is exactly what the authority rule forbids.

Compaction is dispatched on `CompactionAuthority` and never crosses
authorities: there is no fallback engine behind a native harness.

## Test surface

- `vellum-harness-protocol/tests/golden.rs` — wire-format golden corpus.
- `vellum-harness-runtime/tests/registry.rs` — discovery and probe truthfulness.
- `vellum-harness-testkit/tests/supervisor.rs` — process lifecycle.
- `vellum-harness-testkit/tests/pipeline.rs` — UI-to-agent pipeline.
- `vellum-harness-testkit/tests/authority.rs` — one owner per turn, plus a
  structural check that the harness stack has no dependency edge into the proxy
  model runtime or `codex-core`.
- `vellum-harness-testkit/tests/bootstrap_replay.rs` — the pinned bootstrap
  sequence answered end to end.
- `vellum-codex-facade/tests/schema_conformance.rs` — every protocol name the
  facade uses exists in the pinned Codex schema.

All of these run against the scripted `fake-acp-agent`; none needs a live
provider or an API key.

## Transport

`transport::serve` runs the facade over any `AsyncRead`/`AsyncWrite` pair as
newline-delimited JSON-RPC, so the same server serves stdio, a pipe or a socket
without changing protocol handling. The connection is bidirectional: besides
answering client requests it originates permission approvals and correlates the
client's JSON-RPC responses back to the facade. The outbound pump is a separate
task, so a client that is slow to answer cannot wedge request handling.

The Vellum permission id doubles as the JSON-RPC id of the approval request —
JSON-RPC permits string ids, and reusing it keeps one correlation key across the
whole round trip. A client that replies with a JSON-RPC *error* is treated as a
refusal and still reaches the harness, so a declined tool call is never left
hanging.

`vellum-app-server` is the stdio entry point:

```bash
vellum-app-server --harness grok-build=/path/to/grok --bindings <sqlite path>
```

A harness that is not passed is not offered; selecting it fails rather than
falling back to another one.

## Codex client integration (plan §60)

The plan asks whether the bundled Codex can be pointed at a remote app server
without forking it. What the installed 0.142.5 actually exposes:

- `codex app-server proxy --sock <PATH>` bridges stdio to an app-server control
  socket. This is the mounting point: a process owning that socket serves the
  client.
- `codex app-server daemon <start|stop|version>` **is Unix-only**. On Windows it
  fails with "app-server daemon lifecycle is only supported on Unix platforms".
  The `proxy` client itself does run on Windows and targets an AF_UNIX path
  under `~/.codex/app-server-control/`.
- The `tui_app_server` feature flag is `removed / true`, i.e. the TUI always
  goes through the app-server client rather than a separate in-TUI path.

So the seam exists, but on Windows — this project's development platform —
Vellum cannot yet occupy it: binding AF_UNIX from Rust on Windows needs a
crate outside the current workspace (tokio has no `UnixListener` there), and the
daemon lifecycle Codex itself uses is unavailable.

Current status: `vellum-app-server` speaks the protocol correctly over stdio and
is verified end to end against a scripted agent, but nothing yet attaches it to
a socket a real Codex client would dial. That attachment is the remaining work
for a live Grok A/B, and it is platform-specific.
