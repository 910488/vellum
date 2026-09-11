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

The core is not something the user chooses. `verify_settings` matches it byte
for byte against the current Rust target's entry in `artifacts` in
`enhanced-runtime.lock.json`, so exactly one file can pass per platform;
Vellum resolves it instead of asking. It looks, in order, at

1. the managed store, `<data root>/enhanced-runtime/core/sha256_<hex>/codex.exe`,
2. whatever `binaries/vellum-enhanced-codex.dev-path` names (dev builds only),
3. `binaries/vellum-enhanced-codex.exe` beside the installed executable.

A dev build therefore needs one line, written once, naming the fork's output:

```
echo C:/path/to/codex-rs/target/release/codex.exe > src-tauri/binaries/vellum-enhanced-codex.dev-path
```

The pointer exists because the core is a ~300 MB build output; copying it into
`binaries/` on every rebuild is not worth it locally. When neither is present,
Settings says this build does not carry a core — which is a broken install, not
an unfinished setup, and it says so in those words.

### A build without Remote Control

`tauri build` never builds the Remote payload — `scripts/build-local-release.ps1`
does that, and `src-tauri/resources/remote/` is whatever a previous run staged.
But an ordinary build still *ships* it: 535 MB of Linux agent images that a
release not ready to support Remote Control should not be handing to users.

`src-tauri/tauri.no-remote.conf.json` is a config overlay that drops that
resource and keeps `binaries/*`:

```
pnpm tauri build --bundles nsis --config src-tauri/tauri.no-remote.conf.json
```

Arrays are replaced rather than merged, so this leaves the tracked config
honest about what a full release contains instead of editing it per build.

The Remote screen is still present in the UI. Asked to do anything, it reports
`ReleaseManifestUnavailable` — an honest failure, not a crash, but not a hidden
feature either. One caveat when checking this locally: `release_resource_roots()`
also looks at the compile-time `CARGO_MANIFEST_DIR`, so on the machine that
built it the app still finds the manifest in the source tree and behaves as if
the payload shipped. Only a clean machine shows the real behaviour.

### Releases

A release ships the core, so `pnpm run build:sidecar` stages the real file. By
default it downloads the commit-addressed archive for the current Rust host
target from `910488/enhanced-codex-core`, verifies both the archive and core
SHA-256 values from `enhanced-runtime.lock.json`, and caches the verified
extraction under `target/enhanced-runtime/`. A local source directory remains
available as an explicit offline/development override:

```
VELLUM_ENHANCED_CODEX_DIR=C:/path/to/codex-rs/target/release pnpm run build:sidecar
```

It copies the platform core and its applicable helpers, then refuses to
continue unless their pinned archive and core identities match. Failing at
this point is deliberate: the identical
mismatch discovered at run time is a user looking at a settings screen with no
control on it that can fix the problem.

A missing applicable helper fails a release build. The Windows sandbox pair
decides whether shell commands work at all; `codex-code-mode-host` supplies
code mode on every supported Desktop platform. All of them end up in
`src-tauri/binaries/`, which `tauri.conf.json` already bundles as
`binaries/*`, and which is where Codex looks for siblings of its own binary.

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

This builds and embeds the complete Remote Manager deployment payload for
Linux amd64 and arm64, downloads and verifies the Enhanced Core matching the
Mac's Rust host target (`aarch64-apple-darwin` or
`x86_64-apple-darwin`), and then creates the DMG. A small DMG without these
generated resources or the pinned Core is not a release build.

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

Before the Tauri NSIS step, `build:main` always runs
`scripts/build-local-release.ps1 -Mode stage-only`. That rebuilds Linux
amd64 and arm64 Agent, Broker, Codex, and the proxy image from the current
source fingerprint. `-Resume` is refused. Missing `manifest.json`, a missing
architecture artifact, a `releaseVersion` other than `package.json`, or a
proxy image that does not match this source fail the installer build. A
checkout that only contains `resources/remote/README.md` cannot ship.

A named clean repair worktree may be passed with `-SourceWorktree` and
`-AllowDevelopment` for validation. That artifact is recorded as
`development` under an isolated directory and must not be labeled a main
release.

The Tauri step also downloads and verifies the pinned
`x86_64-pc-windows-msvc` Enhanced Core archive. A manual dev pointer is not
required for a release build.

### Do not use this loop to develop Remote Manager

Everything above exists to produce a release, and it rebuilds both
architectures, both proxy images, and re-downloads the pinned Codex every
time because a release must be buildable from nothing. Iterating on the
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
installer. Re-run `build-local-release.ps1` before building a release.

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
clean status, release kind, build time, version, executable SHA-256,
installer SHA-256, and the Remote Manager payload identity described above.
A development build never writes under `artifacts/main/`.

`build:local-release -Resume` is refused unless the cached proxy image
fingerprint already matches the current source tree. `build:main` and
`build:local-release -Mode stage-only` never pass `-Resume`. Fresh release
builds do not pass `-Resume`.

```powershell
pnpm run test:build-remote
```

That script is the fail-closed gate for missing, stale, and mismatched
remote payloads. It also asserts `build:main` stages the payload before
`pnpm run build`.
