# Windows Sandbox QA lane

Operational. Does not override `protocol-source-of-truth.md`.

```text
pnpm qa -- --lane sandbox --build-info path/to/build-info.json
```

`--build-info` is required. The installer is the unique file named in that JSON after SHA verification. Newest-mtime selection is forbidden. `releaseKind` other than `main` is labelled a development candidate.

## Isolation

Each run gets a new run ID, output directory, and ownership lock (`qa/runs/sandbox.lock`). Another run’s Sandbox is never closed.

Windows allows only one Sandbox VM. If `vmmemWindowsSandbox` / `WindowsSandboxRemoteSession` / `WindowsSandboxServer` is already running, the lane does **not** call `WindowsSandbox.exe` again (that produces `僅允許一個正在執行的 Windows 沙箱執行個體`). It records `BLOCKED` / `launcher-unavailable` with the process list. A leftover diagnostic VM that cannot be closed without elevation is an environment block, not a PASS.

Logon uses `cmd.exe /c C:\SandboxKit\guest-start.cmd` (no nested PowerShell quotes). The guest waits for mapped `run-manifest.json` then writes `HEARTBEAT`. The host waits for that file (up to 5 minutes), not for `WindowsSandboxClient` after 8 seconds.

Mapped-folder executables can be blocked by WDAC inside Sandbox. The guest copies the named installer to `C:\QaTools`, re-checks SHA, then runs it. Guest JSON is written without a UTF-8 BOM.

Two clean VMs run in order (Vellum-first, then Codex-first with uninstall last). The owned VM is closed between scenarios only after this run’s HEARTBEAT was seen.

The generated `.wsb` maps only:

| Host | Sandbox | Access |
| --- | --- | --- |
| staged kit | `C:\SandboxKit` | read-only |
| the one installer | `C:\Installers` | read-only |
| this run’s output | `C:\QaOutput` | read/write |
| encrypted credential envelope | `C:\QaSecrets` | read-only |
| one-time wrap key dir | `C:\QaOnce` | read/write, destroyed after the VM |

The git repo, host `~/.codex`, host Vellum data, and the Docker socket are not mapped.

## Credentials

The host resolver reads env (`VELLUM_QA_OPENCODE_KEY`, `VELLUM_QA_GROK_KEY`, `VELLUM_QA_QWEN_KEY`). Secrets are sealed with a one-time AES-256-GCM envelope. The wrap key is **not** placed in `C:\QaOutput` or beside the envelope as `key.b64`; it is `C:\QaOnce\once.key`, unwrapped then deleted before install. After Vellum starts, the guest types the OpenCode key through the add-provider Edit control (`-SetValueFile`, never argv) and looks for `credentials\*.bin` under the VM’s own data dir. Codex/Grok interactive login remains a manual checkpoint. Inject does not PASS the login UI.

## Models and budget

Live cases expand to OpenCode MiMo 2.5, Grok 4.6, and Qwen. Silent substitution is `FAIL` / `wrong-model`. The intended budgets are 200K / 100K / unlimited for the whole round, including subagents, review, compaction, and retries.

The Sandbox lane currently does not dispatch paid live calls. Its former guest script used whichever `codex` executable appeared first, did not bind an expanded row to its pinned model, did not reserve the round budget, and inferred subagent/review success from model-written files. Those observations cannot prove Vellum Enhanced behavior. Live rows therefore remain `NOT_RUN` / `case-driver-unavailable` until a Vellum-bound adapter exports the exact case/model identity, a completed native session event stream, provider usage, real child IDs, and formal review events. Setting `VELLUM_QA_LIVE=1` cannot change that verdict.

## Remote from Sandbox

`scripts/qa/remote/dedicated-host.ps1` creates `vellum-qa-<runId>` with its own port, SSH key, and known-hosts. It does not reuse `vellum-dev-host`. Keep the host until live task + resume finish, then `down`.

## macOS / Official live

Still excluded this round.
