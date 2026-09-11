# Remote Manager clean-host acceptance

This gate mutates only the SSH host explicitly selected by the operator. It
never deletes an existing Vellum installation to manufacture a clean result.

## Build the installer

Docker Desktop with buildx, Rust, Node.js, and pnpm must be installed on the
Windows build machine. Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass `
  -File .\scripts\build-local-release.ps1 `
  -CodexVersion 0.147.0
```

The script builds Linux amd64/arm64 Agent and Broker binaries, downloads the
pinned Codex binaries, exports a platform-specific Proxy Docker archive,
generates the schema-v3 manifest, and builds one NSIS installer containing the
complete deployment bundle. The manifest hash is embedded into the executable.
No GitHub release, GitHub Actions, GHCR, or remote artifact server is used.

## Prerequisites for the target

- The host is reachable through a Codex App/OpenSSH alias.
- The host runs Linux `amd64` or `arm64`.
- Docker and user systemd are available. Bootstrap enables and verifies user
  lingering before installing Codex; a host policy that refuses this action
  fails closed with `lingerEnableFailed`.
- The host has no Vellum Agent, Broker, managed state, or Vellum user service.
- Provider credentials and at least one qualified model are configured in
  Vellum Desktop.

## UI qualification

1. Install the generated NSIS package.
2. Open **Remote Manager**, select the pristine SSH host, and confirm Release
   trust reports the bundled version as ready.
3. Click **部署此主機**. The operation must progress through
   `cleanHostPreflight`, `hostPreflight`, component/Codex installation,
   `nativeDaemon`, `deploymentPlan`, `deploymentApply`, `verification`, and
   finish at `verified` with `detachedReady`.
4. Open a native remote project in Codex App. Confirm the injected catalog is
   visible and complete one provider request.
5. Close Codex App while a long turn is running, reopen it after completion,
   and confirm the same task can be resumed.
6. Return to Remote Manager and click **解除 Vellum 管理…**. It must refuse an active
   turn; otherwise it restores the original config/catalog, restarts the
   native daemon when necessary, stops the Proxy, and finishes at `restored`.
7. Confirm Agent, pinned Codex, tasks, and user data remain present. Only
   Vellum-managed routing is withdrawn.

The bootstrap fails closed if the embedded manifest changes, an artifact hash
does not match, the host is not pristine, the Proxy image cannot be loaded, or
the resulting native daemon and Proxy do not reach `detachedReady`.
