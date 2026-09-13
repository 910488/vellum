#!/usr/bin/env bash
set -euo pipefail

codex_version="${CODEX_VERSION:-0.147.0-alpha.6.6}"
mode="${1:-stage-only}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_root="$repo/src-tauri/resources/remote"
scratch_root="$repo/target/local-release"
version="$(node -p "require('$repo/package.json').version")"
resume="${VELLUM_RELEASE_RESUME:-0}"

case "$mode" in
  stage-only) ;;
  *) echo "usage: $0 [stage-only]" >&2; exit 2 ;;
esac

command -v docker >/dev/null || { echo "Docker with buildx is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v node >/dev/null || { echo "Node.js is required" >&2; exit 1; }

# Docker Desktop's default `docker` buildx driver cannot export
# `type=docker,dest=...` archives unless its containerd image store is enabled.
# Use an isolated container builder per architecture so local macOS builds and
# CI produce the same loadable proxy-image.tar without mutating the user's
# local image tags. Removing each builder immediately also bounds disk usage on
# developer Macs instead of retaining both architecture caches until the end.
builder=""
cleanup_builder() {
  if [[ -n "$builder" ]]; then
    docker buildx rm --force "$builder" >/dev/null 2>&1 || true
    builder=""
  fi
}
trap cleanup_builder EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

sha256() {
  if command -v shasum >/dev/null; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

sha256_stdin() {
  if command -v shasum >/dev/null; then
    shasum -a 256 | awk '{print $1}'
  else
    sha256sum | awk '{print $1}'
  fi
}

proxy_source_fingerprint() {
  while IFS= read -r relative; do
    printf '%s\0%s\n' "$relative" "$(sha256 "$repo/$relative")"
  done < <(git -C "$repo" ls-files -- \
    Cargo.toml Cargo.lock crates src-tauri/Cargo.toml src-tauri/src \
    third_party/codex-0.150 deploy/proxy/Dockerfile deploy/remote-bundle/Dockerfile | sort)
}

proxy_fingerprint="$(proxy_source_fingerprint | sha256_stdin | cut -c1-12)"
[[ -n "$proxy_fingerprint" ]] || { echo "unable to fingerprint proxy image sources" >&2; exit 1; }
proxy_image="vellum-proxy:$version-$proxy_fingerprint"

complete_arch() {
  local arch_root="$1"
  local name
  for name in vellum-remote-agent vellum-remote-broker codex proxy-image.tar; do
    [[ -s "$arch_root/$name" ]] || return 1
  done
  [[ -s "$arch_root/proxy-image.ref" ]] || return 1
  [[ "$(tr -d '\r\n' < "$arch_root/proxy-image.ref")" == "$proxy_image" ]] || return 1
}

if [[ "$resume" != "1" ]]; then
  rm -rf "$scratch_root"
fi
mkdir -p "$scratch_root" "$resource_root"

for arch in amd64 arm64; do
  arch_root="$scratch_root/$arch"
  if [[ "$resume" == "1" ]] && complete_arch "$arch_root"; then
    echo "==> Reuse completed Linux $arch payload"
    continue
  fi
  rm -rf "$arch_root"
  mkdir -p "$arch_root"
  builder="vellum-release-$$-$arch"
  docker buildx create --name "$builder" --driver docker-container >/dev/null
  docker buildx inspect "$builder" --bootstrap >/dev/null

  echo "==> Build Linux $arch Agent and Broker"
  docker buildx build \
    --builder "$builder" \
    --platform "linux/$arch" \
    --file "$repo/deploy/remote-bundle/Dockerfile" \
    --target export \
    --output "type=local,dest=$arch_root" \
    "$repo"

  if [[ "$arch" == arm64 ]]; then
    codex_target="aarch64-unknown-linux-musl"
  else
    codex_target="x86_64-unknown-linux-musl"
  fi
  codex_archive="$arch_root/codex.tar.gz"
  codex_url="https://github.com/openai/codex/releases/download/rust-v$codex_version/codex-$codex_target.tar.gz"
  echo "==> Download pinned Codex $codex_version for $arch"
  curl -fsSL --proto '=https' --tlsv1.2 "$codex_url" -o "$codex_archive"
  tar -xzf "$codex_archive" -C "$arch_root" "codex-$codex_target"
  mv "$arch_root/codex-$codex_target" "$arch_root/codex"
  rm "$codex_archive"

  echo "==> Build Proxy image archive for $arch"
  docker buildx build \
    --builder "$builder" \
    --platform "linux/$arch" \
    --file "$repo/deploy/proxy/Dockerfile" \
    --tag "$proxy_image" \
    --output "type=docker,dest=$arch_root/proxy-image.tar" \
    "$repo"
  printf '%s\n' "$proxy_image" > "$arch_root/proxy-image.ref"
  cleanup_builder
done

cleanup_builder
trap - EXIT INT TERM

for arch in amd64 arm64; do
  destination="$resource_root/linux-$arch"
  rm -rf "$destination"
  mkdir -p "$destination"
  for name in vellum-remote-agent vellum-remote-broker codex proxy-image.tar; do
    cp "$scratch_root/$arch/$name" "$destination/$name"
  done
done

export VELLUM_RELEASE_REPO="$repo"
export VELLUM_RELEASE_RESOURCES="$resource_root"
export VELLUM_RELEASE_VERSION="$version"
export VELLUM_RELEASE_CODEX_VERSION="$codex_version"
export VELLUM_RELEASE_PROXY_IMAGE="$proxy_image"
export VELLUM_RELEASE_AGENT_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo/crates/vellum-remote-agent/Cargo.toml" | head -1)"
export VELLUM_RELEASE_BROKER_VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo/crates/vellum-remote-broker/Cargo.toml" | head -1)"

node <<'NODE'
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');

const root = process.env.VELLUM_RELEASE_RESOURCES;
const artifact = (relative) => ({
  url: `bundle://${relative}`,
  sha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(root, relative))).digest('hex'),
});
const manifest = {
  schemaVersion: 3,
  releaseVersion: process.env.VELLUM_RELEASE_VERSION,
  codex: {
    pinnedVersion: process.env.VELLUM_RELEASE_CODEX_VERSION,
    compatibleRange: process.env.VELLUM_RELEASE_CODEX_VERSION,
    artifacts: {
      'linux-x64': artifact('linux-amd64/codex'),
      'linux-arm64': artifact('linux-arm64/codex'),
    },
  },
  agent: {
    version: process.env.VELLUM_RELEASE_AGENT_VERSION,
    artifacts: {
      'linux-x64': artifact('linux-amd64/vellum-remote-agent'),
      'linux-arm64': artifact('linux-arm64/vellum-remote-agent'),
    },
  },
  broker: {
    version: process.env.VELLUM_RELEASE_BROKER_VERSION,
    artifacts: {
      'linux-x64': artifact('linux-amd64/vellum-remote-broker'),
      'linux-arm64': artifact('linux-arm64/vellum-remote-broker'),
    },
  },
  proxy: {
    image: process.env.VELLUM_RELEASE_PROXY_IMAGE,
    artifacts: {
      'linux-x64': artifact('linux-amd64/proxy-image.tar'),
      'linux-arm64': artifact('linux-arm64/proxy-image.tar'),
    },
  },
  protocolVersion: 3,
};
fs.writeFileSync(path.join(root, 'manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
NODE

echo "==> Remote package payload"
du -sh "$resource_root"
for arch in amd64 arm64; do
  for name in vellum-remote-agent vellum-remote-broker codex proxy-image.tar; do
    test -s "$resource_root/linux-$arch/$name"
  done
done
test -s "$resource_root/manifest.json"
