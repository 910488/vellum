#!/usr/bin/env bash
# Adds the Darwin ARM64 Agent, native Proxy, and pinned Codex runtime package
# to a resources/remote tree that already holds the Linux payload and its
# manifest, then folds their digests into that manifest. Runs on a macOS host;
# the Linux half comes from `build-local-release.sh stage-only`, which needs
# Docker and so runs elsewhere.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_root="${1:-$repo/src-tauri/resources/remote}"
manifest="$resource_root/manifest.json"
test -s "$manifest" || { echo "missing Linux remote manifest at $manifest" >&2; exit 1; }

codex_version="$(node -p "JSON.parse(require('fs').readFileSync(process.argv[1], 'utf8')).codex.pinnedVersion" "$manifest")"
target_dir="${CARGO_TARGET_DIR:-$repo/target}"

cargo build \
  --manifest-path "$repo/Cargo.toml" \
  -p vellum-remote-agent \
  -p vellum-proxy-daemon \
  --release \
  --target aarch64-apple-darwin

darwin_root="$resource_root/darwin-arm64"
rm -rf "$darwin_root"
mkdir -p "$darwin_root"
cp "$target_dir/aarch64-apple-darwin/release/vellum-remote-agent" "$darwin_root/vellum-remote-agent"
cp "$target_dir/aarch64-apple-darwin/release/vellum-proxy-daemon" "$darwin_root/vellum-proxy-daemon"
node "$repo/scripts/download-codex-package.mjs" "$codex_version" aarch64-apple-darwin "$darwin_root/codex-package.tar.gz"
node "$repo/scripts/finalize-remote-darwin-manifest.mjs" "$resource_root"

for name in vellum-remote-agent vellum-proxy-daemon codex-package.tar.gz; do
  test -s "$darwin_root/$name"
done
