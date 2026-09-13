# Building Vellum

`main` is the single desktop source branch for both Windows and macOS. Every
push that changes desktop code runs the `Desktop Builds` GitHub Actions
workflow and publishes both installers as workflow artifacts.

Proxy and provider changes must follow `docs/protocol-source-of-truth.md`.

## The Enhanced Runtime bridge sidecar

`pnpm run build:sidecar` builds the `vellum-codex-app-server` package (not the
Desktop Tauri package) and stages it into `src-tauri/binaries/`, where the
bundler picks it up as a resource. Identical bytes are not copied again, so
`build.rs` does not see a fresh timestamp. It runs automatically as part of
`beforeBuildCommand`, so a normal `pnpm run build` covers it; run it by hand
only when producing an installer some other way.

`pnpm run dev` similarly runs `build:sidecar:dev` before Tauri compiles the
development host. Debug sidecars are hash-addressed under
`src-tauri/binaries/dev/`, so a bridge still serving Codex Desktop cannot lock
the next development build. Do not replace that with a bare
`cargo build -p vellum-codex-app-server --bin vellum-codex-app-server`: that updates `target/debug` but
does not stage the bytes whose digest `build.rs` embeds in the Vellum host,
leaving Settings with a correct fail-closed bridge hash mismatch.

The sidecar matters because Codex Desktop launches `CODEX_CLI_PATH` directly:
Enhanced needs a real standalone executable that ships with Vellum, not a mode
flag on the Vellum binary. `build.rs` records its SHA-256 into the build
manifest, and the app refuses to enable Enhanced against any bridge binary
whose hash does not match what the release shipped.

A plain `cargo build` stages no sidecar; the recorded hash is then empty and
Settings reports the bridge as missing rather than pretending it is there.

## The Enhanced Codex core, and the helpers next to it

The Enhanced core is built from the public, commit-pinned
[`910488/enhanced-codex-core`](https://github.com/910488/enhanced-codex-core)
repository, not from this repository. Its artifact workflow builds:

```
cargo build --release -p codex-cli
cargo build --release -p codex-windows-sandbox   --bin codex-windows-sandbox-setup --bin codex-command-runner
cargo build --release -p codex-code-mode-host
```

The second and third lines are not optional extras on Windows. Codex keeps its Windows
sandbox helpers as separate executables and looks for them **beside its own
binary** at run time — `<dir>/<name>.exe`, or `<dir>/resources/<name>.exe`. When
that lookup misses, it falls back to the bare file name, Windows tries to
resolve it through `PATH`, and the user gets a dialog box reading
`Windows 找不到 'codex-windows-sandbox-setup.exe'` in the middle of a turn.

Building only `codex-cli` therefore produces a core that verifies, injects, and
chats correctly, and then fails at the first sandboxed shell command. Settings
reports the gap as soon as Enhanced is armed rather than leaving it to be
discovered that way, but the fix is here: build the helpers.

### Pointing a development build at the core

The core is not part of the Desktop installer. `verify_settings` matches it
byte for byte against the current Rust target's entry in `artifacts` in
`enhanced-runtime.lock.json`, so exactly one file can pass per platform;
Vellum resolves it instead of asking. It looks, in order, at

1. the managed store, `<data root>/enhanced-runtime/core/sha256_<hex>/codex.exe`,
2. whatever `binaries/vellum-enhanced-codex.dev-path` names (dev builds only),
3. a legacy bundled baseline beside the installed executable, when upgrading
   an older Vellum installation.

A dev build therefore needs one line, written once, naming the fork's output:

```
echo C:/path/to/codex-rs/target/release/codex.exe > src-tauri/binaries/vellum-enhanced-codex.dev-path
```

The pointer exists because the core is a ~300 MB build output; copying it into
`binaries/` on every rebuild is not worth it locally. A clean Desktop install
has no core until the user installs a signed `core-v*` update. The managed slot
then becomes the normal runtime; the legacy bundled path is compatibility only.

### Desktop, Remote, and Core package boundaries

The three update streams are intentionally independent:

- `desktop-v*` contains the local Tauri proxy application and the small
  `vellum-codex-app-server` bridge used to launch an independently installed
  Core. It contains no Linux image, Remote Agent/Broker/Codex payload, Enhanced
  Core binary, sandbox helper, or code-mode helper.
- `remote-v*` contains the Linux Remote Agent, Broker, pinned Codex CLI,
  `proxy-image.tar`, and the metadata needed to install them on an SSH host.
- `core-v*` contains the Enhanced Codex Core and the platform helpers it needs.

`tauri.conf.json` enforces the Desktop boundary by allowing only the bridge
resource glob. Staging files under `src-tauri/resources/remote/` or additional
binaries under `src-tauri/binaries/` therefore cannot silently add them to a
Desktop installer.

### Releases

`pnpm run build:sidecar` builds and stages only the Desktop bridge. It does not
download or copy an Enhanced Core or any Core helpers. Desktop release jobs do
not build or download the Remote payload either.

Independent Core updates use `core-vX.Y.Z` or `core-vX.Y.Z-rc.N` tags in this
repository. The release workflow downloads every commit-pinned archive from
`910488/enhanced-codex-core`, checks its archive hash from
`enhanced-runtime.lock.json`, safely extracts it, and signs a schema-2 update
manifest containing the relative path, size, SHA-256, and executable role of
the core and every helper. Each target's `protocolSchemaSha256` is taken by
probing the extracted `codex` binary (`app-server generate-json-schema`), not
copied from the lock fixture. `protocolVersion` in the lockfile must still
match the checked-in protocol fixture. The workflow only creates a draft;
publishing remains a separate release decision.

At download time Vellum verifies the signed manifest and archive, rejects
links, traversal paths, undeclared files, missing helpers, size differences,
and per-file hash differences, then writes an immutable pending slot. The
active slot changes only after Codex restarts through that pending executable
and the bridge attestation confirms the launch. A failed launch drops the
pending slot; an explicit rollback keeps the reverted build as history without
automatically scheduling it again.

### Automated nightly and manual releases

`Nightly and manual layered releases` is the end-to-end GitHub Actions entry
point. Its daily schedule builds the long-lived `codex/dev` branch, resolves
the latest published commit-addressed Enhanced Core release once, and
publishes matching preview releases for the independently updatable Core and
Desktop layers. Nightly versions use
`<next-patch>-nightly.<UTC-date>.<run-number>.<run-attempt>`, so Stable clients
ignore them, Preview clients can select them, and a rerun remains immutable.

The workflow can also be run manually. A Stable run requires an explicit
non-prerelease SemVer; a Preview run accepts one or generates the nightly
version. `source_ref` selects the Vellum source, and `enhanced_core_release`
can pin an explicit `vellum-core-<40-hex>` release instead of resolving the
latest one. Turning off `publish` leaves both signed releases as drafts.

For testing one hot-update surface independently, run the
`Publish signed hot update` Action. Its `component` selector maps directly to
the three updater streams: `desktop` builds the Vellum app, `remote` builds the
Remote package, and `core` builds the Enhanced Codex Core package. Choose
`pre-release` for a Desktop Preview-channel test or `release` for the Stable
channel. A release requires an explicit non-prerelease SemVer; a prerelease
may omit the version, in which case the workflow generates a unique next-patch
`preview` version. Keep `publish` enabled for an installed Desktop to discover
the result—draft releases are intentionally invisible to update checks.

The Core release completes before the Desktop build starts, so its signed lock
identity can be recorded by Desktop without embedding the Core bytes.

`codex-code-mode-host` links `rusty_v8` and downloads a prebuilt archive from
GitHub. If that download fails, the rest of the build still succeeds and only
code mode is affected; retry, or set `V8_FROM_SOURCE=1` to compile V8 locally.

## Checking the Enhanced Runtime without doing it by hand

Four commands, in the order their answers stop being cheap. Each one is a
different question; running the expensive one first only tells you the same
thing more slowly.

```
pnpm gate:status      is Enhanced usable on this machine right now?
pnpm gate:bridge      does the bridge itself behave?
pnpm gate:live        does it hold up against a real model?
pnpm gate:installed   does the Codex Desktop you launch actually go through it?
```

`gate:status` reads only — no lease, no restart, nothing written. It prints the
four things that have to be true before any button works, then the paths behind
them, and exits non-zero when Enhanced is not ready. The paths are the point: a
state word like `environmentDrift` cannot be checked by hand, but
`CODEX_CLI_PATH`, `configuredBridge` and `liveBridge` can. `--json` emits the
same reading for scripts.

`gate:bridge` starts the real bridge binary against App Server protocol
children and a scripted local provider, and drives routing, binding, tool
reliability, pruning, overflow, bounded continuation, cancel and steer, resume
across a bridge restart, and the fail-closed paths. About a minute, no Codex
Desktop, no provider quota, no environment variable touched. It reports
`COMPONENT PASS` rather than `GO`, because the children are surrogates: it
proves the bridge is right, never that a release is shippable.

`gate:live` is `gate:bridge`'s runtime with a real endpoint on the far end
instead of the scripted provider, so the Enhanced ports run over real model
output: call ids the model chose, a refusal in the server's own words, a
context window that is actually finite.

```
pnpm gate:live --provider-url http://127.0.0.1:8000 --provider-model qwen
```

Both arguments are required and neither is guessed. A catalog id is a Vellum
routing key and means nothing to a provider, so every request is rewritten to
`--provider-model` on the way out.

The endpoint must serve the Responses API (`POST /v1/responses`) with tool
calling. A recording pass-through sits in front of it and does three things: it
keeps every observation the scripted provider makes — most importantly that no
third-party request carried a remote compaction trigger — it translates the
gate child's abbreviated item shape into canonical Responses items, and it maps
a provider's own name for a context overflow onto the one the runtime acts on,
without touching the message. The report keeps every exchange, so each case
detail can be checked against what the server actually returned.

Like `gate:bridge` it is never promotion evidence: the children are still
surrogates, so what this proves real is the provider edge, not the Codex agent
loop. Bounded continuation is deliberately not in this lane for the same
reason — its trigger is Codex's own plan and subagent state, which no provider
can produce, and the gate child's mapping of a truncated response onto an
unfinished signal is a scripting convenience rather than the port's contract.
It stays covered by `gate:bridge`.

`gate:installed` restarts the packaged Codex Desktop through Vellum's managed
restart, twice, and refuses to call anything a success until the bridge
belonging to that launch attests it is ready with both children up. It also
refuses to start until you confirm no turn is in flight, which a CLI cannot
read for itself:

```
pnpm gate:installed --no-active-turn
```

Its first four checks are the ones `gate:status` prints, from the same code. A
machine the status command refuses cannot produce a green qualification report
by another route.

### What a green run is allowed to claim

Only `gate:installed` prints `GO`. Everything else prints `COMPONENT PASS` and,
after it, the reason it is not promotion evidence — `surrogate-children` for a
default `gate:bridge` run, `live-provider:<model>` for `gate:live`.

`gate:bridge` also takes `--official-binary` and `--enhanced-binary` to run the
smoke path against real Codex builds instead of the surrogates. Passing them is
not by itself a promotion claim, because a path proves only that a file exists.
A run is promotion-eligible only when the two binaries are distinct and the
enhanced one is the artifact this machine is pinned to; otherwise the footer
names what disqualified it:

| Footer | What happened |
| --- | --- |
| `surrogate-children` | the default: at least one plane is `vellum-codex-gate-child` |
| `one-binary-serving-both-planes` | both flags point at the same file, so the isolation cases are asserting that one process is both dead and alive |
| `enhanced-binary-is-not-pinned-on-this-machine` | nothing is configured to compare against, which is the normal state on CI |
| `enhanced-binary-is-not-the-pinned-artifact` | the binary under test is not the one the user will launch |

### The bridge that outlives its build

Codex Desktop keeps the bridge process it started until Codex Desktop itself
restarts. That produces two symptoms with one cause.

The runtime one: a fresh launch manifest, a live attestation naming both
children, and every turn served by a binary you replaced hours ago. The
attestation records the children, never which bridge wrote it, so it looks
healthy. `gate:status` compares the running process against the configured
path and both gates fail on the difference.

The build one: `cargo build -p vellum-codex-app-server --bin vellum-codex-app-server` cannot replace
`target/debug`, and cargo reports only `failed to remove file` with an
access-denied error naming nothing. Every gate command therefore compiles into
`target/sidecar-build`, the same isolated directory `build-sidecar.mjs` uses,
and runs from there. Nothing the gate builds touches `target/debug`, so a live
bridge cannot block a run at all — not the bridge's own link step and not the
reader's. It does not stage either: staging rewrites the dev sidecar pointer the
app was built against, and a command whose job is to say whether Enhanced works
must not be able to invalidate the configuration it is reporting on. `pnpm dev`
stages, and should.

One directory is also one compile. Split across two, cargo keeps two caches and
rebuilds the `vellum` library twice per run. A warm `pnpm gate:bridge` is about
twenty seconds, which is the difference between a check you run after every
change and one you skip.

When something else is locked, `scripts/enhanced-gate.mjs` names the process
holding it. It stops nothing — killing a live bridge takes Codex Desktop's CLI
with it, so that stays your call.

## Build macOS locally

On a Mac with Node.js, pnpm, Rust, Docker Desktop, and the Xcode command-line
tools installed:

```bash
pnpm install --frozen-lockfile
pnpm run build:mac
```

This builds the Desktop DMG only. It includes the local proxy and bridge, and
does not build or embed the Remote payload or Enhanced Core.

The DMG is written to:

```text
target/release/bundle/dmg/*.dmg
```

## Build Windows locally

## Build the `main` branch from a multi-worktree checkout

Run this from any Vellum worktree that contains the script:

```powershell
pnpm run build:main
```

`build:main` fetches `origin` and resolves an explicit source ref (default
`origin/main`). A default main-release is produced only when that worktree is
clean, the requested source ref is exactly `origin/main`, and `HEAD` equals
`git rev-parse origin/main`. A dirty or stale `main` checkout, or any custom
`-SourceRef`, fails immediately instead of embedding local edits. Custom refs
can only produce a development artifact when `-AllowDevelopment` is passed.

`build:main` builds the Desktop layer only. It never invokes
`scripts/build-local-release.ps1`, so Docker, the Linux Agent/Broker/Codex
payload, and `proxy-image.tar` are not prerequisites for an NSIS installer.

A named clean repair worktree may be passed with `-SourceWorktree` and
`-AllowDevelopment` for validation. That artifact is recorded as
`development` under an isolated directory and must not be labeled a main
release.

The Tauri step stages the Desktop bridge but does not download an Enhanced
Core. Install and test the signed Core layer separately.

### Do not use this loop to develop Remote Manager

The separate Remote package builder rebuilds both
architectures, both proxy images, and re-downloads the pinned Codex every
time because a Remote release must be buildable from nothing. Iterating on the
Agent, the Broker, or the Desktop's remote code through it wastes most of
that work on artifacts that did not change.

`docs/remote-dev-loop.md` describes the development loop: a disposable
container that offers the three host preconditions Remote Manager checks, a
staging script that rebuilds only what changed, and an end-to-end test that
drives the real Desktop code over real SSH.

```powershell
pnpm run dev:host:up      # boot the disposable Linux target
pnpm run remote:e2e       # Agent/Broker only -- no manifest, no installer
pnpm run remote:e2e:full  # plus manifest verification and pinned Codex
```

A payload staged that way carries a `+dev.<fingerprint>` releaseVersion, so
`Assert-VellumStagedRemotePayload` rejects it and it can never reach an
official Remote package. Re-run `build-local-release.ps1 -Mode stage-only`
before publishing a Remote release.

```powershell
pnpm run build:main
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-main.ps1 -ValidateOnly
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-main.ps1 -SourceWorktree <repair> -AllowDevelopment
```

`-ValidateOnly` checks the source gate only. It never writes or rewrites a
release manifest, so a leftover binary cannot be relabeled as the current
commit. If a commit-addressed artifact manifest already exists, ValidateOnly
compares its commit, version, and release kind against the resolved source.

Tauri and Rust output is commit-addressed and isolated by release kind:

```text
<primary-checkout>/artifacts/<releaseKind>/<commit>/target/
```

The kind-level `build-info.json` is a pointer to that immutable directory.
Useful files are normally found at:

```text
artifacts/main/build-info.json
artifacts/main/<commit>/build-info.json
artifacts/main/<commit>/target/release/vellum-proxy-desktop.exe
artifacts/main/<commit>/target/release/bundle/nsis/*-setup.exe
artifacts/development/<commit>/target/release/vellum-proxy-desktop.exe
```

`build-info.json` records the source ref, full commit, `origin/main` commit,
clean status, release kind, build time, version, executable SHA-256, and
installer SHA-256. Remote package identity belongs to the separate
`remote-v*` release manifest.
A development build never writes under `artifacts/main/`.

`build:local-release -Resume` is refused unless the cached proxy image
fingerprint already matches the current source tree. The default
`build:local-release` script uses `-Mode stage-only` and never passes
`-Resume`. Fresh Remote release builds do not pass `-Resume`.

```powershell
pnpm run test:build-remote
```

That script is the fail-closed gate for missing, stale, and mismatched Remote
payloads. It also asserts `build:main` never stages those payloads into the
Desktop package.
