# Remote deployment resources

`scripts/build-local-release.ps1` generates `manifest.json` plus Linux amd64
and arm64 Agent, Broker, Codex, and Proxy image archives in this directory.
`pnpm run build:main` runs that script in `stage-only` mode first; a Windows
installer without this payload is not a 0.2.3 release.

The build script hashes the generated manifest into the Vellum executable.
At runtime Vellum rejects a resource manifest that differs from the compiled
hash, and verifies every artifact SHA-256 before staging it over SSH.

Generated binaries, archives, and the manifest are intentionally gitignored.
