# Vellum onboarding sandbox

Runs the built Vellum installer plus the official ChatGPT/Codex app in a disposable
Windows Sandbox, so the onboarding flow can be tested as a genuine first run without
touching the host's `~/.codex`, host Vellum state, or the running host Codex app.

Adapted from the CC Switch continuity sandbox kit in
`../../windows-sandbox-test`, which used the same WebView2 + Store bootstrap.

## Run it

```bash
pnpm tauri build
```

Then double-click `Launch-Sandbox.cmd`, or:

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File sandbox/Launch-Sandbox.ps1 -Build
```

`-Build` runs `pnpm tauri build` first. Without it, the newest existing
`src-tauri/target/release/bundle/nsis/*-setup.exe` is used — check the "built N minutes
ago" line so you don't test a stale binary.

## First-time setup

Windows Sandbox is an optional feature and needs enabling once:

1. Save your work.
2. Double-click `Enable-Windows-Sandbox.cmd` and approve the administrator prompt.
3. Restart Windows.

The enable script does not restart automatically unless called with `-Restart`.
The `.cmd` launchers use `ExecutionPolicy Bypass` only for these local scripts, because
this machine blocks direct `.ps1` execution.

## What gets mapped

| Host path | Sandbox path | Access |
| --- | --- | --- |
| `vellum/sandbox` | `C:\SandboxKit` | Read-only |
| `vellum/src-tauri/target/release/bundle/nsis` | `C:\Installers` | Read-only |
| `vellum/sandbox/exchange` | `C:\Exchange` | Read/write |

## What bootstrap does

1. Creates an isolated Codex home at `C:\IsolatedHome\.codex` and pins `CODEX_HOME` /
   `CODEX_SQLITE_HOME` to it, for the process and the Sandbox user.
2. Installs Microsoft's WebView2 Evergreen Runtime — Sandbox ships without it, and a
   Tauri app will not start without it.
3. Installs the newest `*-setup.exe` from `C:\Installers` with `/S`. Tauri's NSIS bundle
   is `installMode: currentUser`, so this needs no elevation.
4. Installs the ChatGPT/Codex Store package (`9PLM9XGG6VKS`). Sandbox intentionally omits
   the Store and WinGet, so this follows Microsoft's documented Sandbox procedure
   (`Microsoft.WinGet.Client` + `Repair-WinGetPackageManager -AllUsers`), sets the Store
   market to US first (a region-less Sandbox makes the Store return `0x8A15003B`), and
   retries three times. Codex is treated as installed only once its AppX package is
   visible.
5. Launches Codex, then Vellum — in that order, so onboarding's Codex detection probes a
   Codex that is already installed.

Every step is logged to `C:\Exchange\sandbox-<timestamp>.log`. On failure the exception
lands in `C:\Exchange\BOOTSTRAP-FAILED.txt` and, for Store problems, the last five WinGet
diagnostic logs are copied to `C:\Exchange\winget-diagnostics`.

## Isolation boundary

- The host user's `.codex` directory and host Vellum data are not mapped.
- The kit and the installer are mapped read-only.
- Only `exchange` is writable from Sandbox — and it persists on the host, so keep secrets
  out of it.
- Closing the Sandbox destroys its apps, login state, DPAPI keys, registry, and files.

## Replaying onboarding

Onboarding completion is a localStorage flag (`vellum.onboarding.completed.v1`), which
Tauri v2 keeps in the WebView2 profile under `%LOCALAPPDATA%\com.vellum.desktop` — not in
the app data dir. `Reset-Onboarding.ps1` clears both that profile and
`%LOCALAPPDATA%\vellum`, then relaunches Vellum. It refuses to run outside Sandbox.

See `TEST-CHECKLIST.md` for what to actually walk through.
