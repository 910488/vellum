# ZCode Desktop host stdio tap

Operational method for borrowing an already-signed-in ZCode Desktop host
when driving the built-in Start Plan / GLM path from Vellum. This is not a
proxy-protocol contract and does not replace [ADR-009](adr/ADR-009-native-harness-manager.md).

Scripts: [`scripts/zcode-desktop-host-tap/`](../scripts/zcode-desktop-host-tap/).

The spawn override has unblocked the core failure. It is still an
**experimental host tap**, not product Vellum wiring. See
[Remaining work](#remaining-work-for-product-wiring).

## Status

This method does **not** let Vellum forge `headersApplied`. It intercepts
the process Desktop already uses to start app-server:

```text
ZCode Desktop
  → tap.mjs
  → real zcode.cjs app-server
```

Desktop remains the owner of:

- Google / Start Plan sign-in
- CAPTCHA
- `refreshCodingPlanApiKey`
- `session/updateRuntimeModelConfig`
- `respondProviderRuntimeHeaders`

`tap.mjs` only forwards JSONL and can inject `session/send` into an existing
session. GLM no longer fails with `captcha verify failed` because the request
stays inside the real Desktop host.

Proven:

- Built-in GLM can be used from an external inject.
- Aliyun CAPTCHA does not need to be reimplemented.
- OAuth tokens and runtime headers do not need to be copied.
- `session/send` is enough to start a model turn on an existing session.
- GLM-5.3-Flash produced real streaming and usage telemetry.

## Why this path exists

A standalone `zcode.cjs app-server --stdio` can run, and Vellum can answer
`interaction/requestProviderRuntimeHeaders` with `{ "headersApplied": true }`.
That acknowledgement is false for ZCode's built-in Start Plan. The app-server
then calls GLM without Desktop's CAPTCHA-bound runtime headers and fails with
`captcha verify failed`.

ZCode Desktop does **not** expose a stable named pipe or JSON-RPC host for
`refreshCodingPlanApiKey`. Those names are in-process TypeScript methods on
the Electron host. The packaged CLI also has no `--listen` app-server and no
headless coding-plan refresh command.

The host does honor a spawn override. That is the attach point.

## Proven shape

```text
Codex / Vellum
  → JSONL inject (session/send) or Desktop UI
  → ZCode Desktop host (CAPTCHA + refreshCodingPlanApiKey
      + session/updateRuntimeModelConfig
      + respondProviderRuntimeHeaders)
  → stdio tap
  → zcode.cjs app-server --stdio --surface desktop
  → builtin:zai-start-plan / GLM
```

Do not rebuild:

```text
Codex → Vellum → independent app-server → fake headersApplied
```

## Spawn override

Restart ZCode Desktop with these process environment variables. A running
instance will not pick them up; `requestSingleInstanceLock` only focuses the
existing window.

| Variable | Role |
|---|---|
| `ZCODE_AGENT_SERVER_COMMAND` | Executable Desktop spawns instead of `ZCode.exe … zcode.cjs`. Use Node. |
| `ZCODE_AGENT_SERVER_ARGS_JSON` | JSON **array of strings**. Desktop then appends `--surface desktop`. |
| `ZCODE_TAP_LOG_DIR` | Directory for tap logs and `inject-queue.jsonl`. |
| `ZCODE_TAP_INNER_EXE` | Optional. Inner `ZCode.exe`. Default: `%LOCALAPPDATA%\Programs\ZCode\ZCode.exe`. |
| `ZCODE_TAP_INNER_CJS` | Optional. Inner `resources\glm\zcode.cjs`. |
| `ZCODE_TAP_INJECT_PROMPT` | Optional. Auto-sends after `session/resume` only. Leave empty in normal use. |

Desktop's resolver (packaged 3.10.1, no `isPackaged` guard):

```text
command = ZCODE_AGENT_SERVER_COMMAND
args    = JSON.parse(ZCODE_AGENT_SERVER_ARGS_JSON) ?? ["app-server", "--stdio"]
        + ["--surface", "desktop"]
```

The tap ignores those extra args and starts the real app-server itself:

```text
ZCode.exe  resources\glm\zcode.cjs  app-server --stdio --surface desktop --no-color
ELECTRON_RUN_AS_NODE=1
```

Use an **ASCII path** for the tap file. A JSON args string built from a
Unicode OneDrive path was observed to corrupt and prevent Desktop from
staying up. The launcher copies `tap.mjs` to `%TEMP%\zcode-host-tap\`.

Do **not** `fs.watch` the log directory while appending JSONL. That combination
killed the tap on Windows. Poll `inject-queue.jsonl` instead.

## JSONL contract observed on the wire

Envelope is `{ id, method, params }` / `{ id, result }` / `{ id, error }`.
Adding `jsonrpc` is rejected. Host ids are small integers; reverse-request
ids look like `server-N`. Injected client ids should stay in a high range
(the tap uses `900001+`).

Host → app-server (non-exhaustive):

- `workspace/updateProviderRegistry`
- `workspace/readState` (includes `runtimeModel`)
- `v4/conversation/subscribe`
- `v4/command` with `type: "createSession"` (and later `deleteSession`)
- `session/resume`
- `session/read`
- `session/updateRuntimeModelConfig`

App-server → host:

- `session/requestRuntimePreferences`
- `interaction/requestOfficialMcpAuthHeaders`
- `interaction/requestProviderRuntimeHeaders`
- `v4/conversation/frame`
- `v4/telemetry/event`
- `process/mcpTelemetry`

`interaction/requestProviderRuntimeHeaders` params:

```text
{
  requestId, sessionId, turnId?,
  workspace: { workspacePath, workspaceIdentity? },
  modelRef: { providerId, modelId },
  providerId,
  reason: "model-request" | "captcha-retry"
}
```

Host must:

1. Refresh coding-plan credentials in-process (`modelProviderService.refreshCodingPlanApiKey`).
2. Obtain Aliyun CAPTCHA headers when the provider is zcode-plan / Start Plan.
3. Push `session/updateRuntimeModelConfig` with the new runtime model.
4. Answer the reverse request with `{ headersApplied: true }` (optional
   `errorMessage`, `providerRevision`). The CAPTCHA header values stay on
   the host; they are not the public result body.

An injected user turn uses the still-accepted method:

```text
{ "id": 900001, "method": "session/send",
  "params": { "sessionId", "inputId", "content", "toolDenylist": ["Bash","Edit","Write"] } }
```

Desktop's own UI prefers `v4/command` / `v4/conversation/*`. `session/send`
is enough to start a turn on an existing session.

## How to run

Requires a logged-in ZCode Desktop. First launch after setting the env must
kill the previous process tree.

```powershell
pwsh -File scripts/zcode-desktop-host-tap/launch-zcode-with-tap.ps1
```

For normal Vellum use after login, request background companion mode:

```powershell
pwsh -File scripts/zcode-desktop-host-tap/launch-zcode-with-tap.ps1 -Background
```

This hides the initial window and passes Chromium's notification-disable flag.
It does not make Start Plan headless: the ZCode Desktop process must remain
running because refresh and CAPTCHA live in the Electron host. Open ZCode from
its tray icon when CAPTCHA or re-login is required. Packaged-version
qualification must verify that the window stays hidden and notifications are
suppressed; these launch hints are not a stable ZCode API.

Confirm the tree:

```text
ZCode.exe
  → node.exe  %TEMP%\zcode-host-tap\tap.mjs --surface desktop
    → ZCode.exe  resources\glm\zcode.cjs app-server --stdio --surface desktop
```

Logs: `%TEMP%\zcode-host-tap\tap-<pid>.jsonl` (method-level; tokens and
CAPTCHA headers are not stored).

Inject onto the live session without creating another thread:

```powershell
node scripts/zcode-desktop-host-tap/inject-send.mjs --content "Reply with exactly PONG"
```

The formal same-session gate uses the Rust control client and checks the
provider, model, streamed answer, terminal outcome, and stable session id:

```powershell
cargo run -p vellum --bin vellum-eval -- zcode-desktop-canary --repeat 2
```

The default 15-second inter-turn settle is intentional. ZCode may start a
host-owned title/summary model request just after the visible turn completes;
the Start Plan account can serialize that background request with an immediate
next user turn. Both canary turns still use the exact same session id.

To expose the driver through the Codex facade after the tap is ready:

```powershell
cargo run -p vellum-codex-facade --bin vellum-app-server -- --harness zcode-desktop
```

The helper discovers the newest listener and uses the control pipe directly.
Pass `--session-id` to pin a session. The queue file remains a compatibility
fallback only.

## Product wiring layer

The crate `vellum-zcode-desktop` and harness driver `ZcodeDesktopDriver`
implement the first product-facing API:

```text
Vellum ZCode adapter
  ↔ named pipe / unix socket  (hello, bind, turn/start, turn/cancel, status)
  ↔ Desktop host tap
  ↔ ZCode app-server
```

Control protocol version is `2`. Discovery file: `%TEMP%\zcode-host-tap\tap-<pid>.listen.json`.
Bindings persist in `thread-bindings.sqlite` keyed by Vellum thread id.
`hello` may carry `expectedCjsSha256`; a mismatch fails closed.
`turn/start` rejects a duplicate in-flight `vellumTurnId`. Events
`turn/started`, `turn/delta`, `assistant/completed`, `turn/usage`, tool
lifecycle, and `turn/completed` correlate that id. Lifecycle events cover
`ready`, `captcha-waiting`, `app-server-restart`, and `exited`.

Current Codex-facing qualification:

| Capability | Status |
|---|---|
| thread bind, list, resume | implemented and same-session tested |
| turn start, streaming, completed message, completion | implemented and live tested |
| usage | implemented from ZCode telemetry |
| tool start/update/completion | implemented and live `Read` tested |
| cancellation | implemented; crate-tested |
| model/reasoning selection | unsupported for built-in Start Plan until its host command is qualified |
| permissions for write/shell | unsupported; tap currently denies `Bash`, `Edit`, and `Write` |
| plans, compaction, commands, terminal, subagents | not yet qualified from ZCode wire evidence |

`inject-queue.jsonl` remains a fallback for scripts. Prefer
`inject-send.mjs` (control pipe) or `ZcodeDesktopHost`.

Still open before Desktop UI promotion:

- Register `ZcodeDesktopDriver` in the production harness registry / settings.
- Qualify each ZCode upgrade with a new `cjsSha256` pin.
- End-to-end CAPTCHA-waiting UI.
- Add the Desktop settings/provider selector entry; the driver and Codex
  facade registration exist, but the product UI does not expose it yet.
- Confirm live conversation, tool, and completion extraction on each new
  artifact.

## Out of scope / do not use

- Independent app-server plus a fake `{ "headersApplied": true }`
- `\\.\pipe\zcode-node-repl-*` (token-gated browser broker)
- `\\.\pipe\zcode-cua-helper-*` (computer-use only)
- Packaged `--remote-debugging-port` (disabled)
- Copying OAuth material out of `.zcode/v2/config.json` as a Start Plan
  substitute
- Re-implementing Aliyun CAPTCHA (`X-Aliyun-Captcha-Verify-Param`)
- `zcode://` deep links (oauth callback and `workspace/open` only)

Web Remote Control (`zcode:start-web-remote-control`, relay WebSocket) is a
second official “external client uses Desktop host” surface. It was not used
for this attach method.
