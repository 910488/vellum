#!/usr/bin/env bash
set -euo pipefail

codex_version="${CODEX_VERSION:-0.147.0-alpha.6.6}"
bundle="${1:-dmg}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
resource_root="$repo/src-tauri/resources/remote"
scratch_root="$repo/target/local-release"
version="$(node -p "require('$repo/package.json').version")"
resume="${VELLUM_RELEASE_RESUME:-0}"

case "$bundle" in
  dmg|nsis|stage-only) ;;
  *) echo "usage: $0 [dmg|nsis|stage-only]" >&2; exit 2 ;;
esac

command -v docker >/dev/null || { echo "Docker with buildx is required" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v node >/dev/null || { echo "Node.js is required" >&2; exit 1; }

if [[ "$bundle" != "stage-only" ]]; then
  bash "$repo/scripts/check-main-ancestry.sh"
fi

# Build native musl payloads on the builder's architecture; no target QEMU compilation.
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
    third_party/codex-0.150 deploy/proxy/Dockerfile deploy/remote-bundle/Dockerfile scripts/build-local-release.ps1 scripts/build-local-release.sh scripts/finalize-remote-linux-manifest.mjs | sort)
}

proxy_fingerprint="$(proxy_source_fingerprint | sha256_stdin | cut -c1-12)"
[[ -n "$proxy_fingerprint" ]] || { echo "unable to fingerprint proxy image sources" >&2; exit 1; }
proxy_image="vellum-proxy:$version-$proxy_fingerprint"

complete_arch() {
  local arch_root="$1"
  local name
  for name in vellum-remote-agent vellum-proxy-daemon codex; do
    [[ -s "$arch_root/$name" ]] || return 1
  done
  [[ -s "$arch_root/native-source.ref" ]] || return 1
  [[ "$(tr -d '\r\n' < "$arch_root/native-source.ref")" == "$proxy_image:$codex_version" ]] || return 1
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

  echo "==> Cross-compile Linux $arch musl Agent and native Proxy"
  docker buildx build \
    --builder "$builder" \
    --build-arg "REMOTE_ARCH=$arch" \
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

  printf '%s\n' "$proxy_image:$codex_version" > "$arch_root/native-source.ref"
  cleanup_builder
done

cleanup_builder
trap - EXIT INT TERM

for arch in amd64 arm64; do
  destination="$resource_root/linux-$arch"
  rm -rf "$destination"
  mkdir -p "$destination"
  for name in vellum-remote-agent vellum-proxy-daemon codex; do
    cp "$scratch_root/$arch/$name" "$destination/$name"
  done
done

agent_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo/crates/vellum-remote-agent/Cargo.toml" | head -1)"
node "$repo/scripts/finalize-remote-linux-manifest.mjs" "$resource_root" "$version" "$codex_version" "$agent_version" "$proxy_fingerprint"

if [[ "$(uname -s)" == Darwin ]]; then
  echo "==> Build Darwin ARM64 Agent and native Proxy"
  rustup target add aarch64-apple-darwin
  cargo build \
    --manifest-path "$repo/Cargo.toml" \
    -p vellum-remote-agent \
    -p vellum-proxy-daemon \
    --release \
    --target aarch64-apple-darwin
  darwin_root="$resource_root/darwin-arm64"
  rm -rf "$darwin_root"
  mkdir -p "$darwin_root"
  cp "${CARGO_TARGET_DIR:-$repo/target}/aarch64-apple-darwin/release/vellum-remote-agent" \
    "$darwin_root/vellum-remote-agent"
  cp "${CARGO_TARGET_DIR:-$repo/target}/aarch64-apple-darwin/release/vellum-proxy-daemon" \
    "$darwin_root/vellum-proxy-daemon"
  codex_archive="$scratch_root/codex-darwin-arm64.tar.gz"
  curl -fsSL --proto '=https' --tlsv1.2 \
    "https://github.com/openai/codex/releases/download/rust-v$codex_version/codex-aarch64-apple-darwin.tar.gz" \
    -o "$codex_archive"
  tar -xzf "$codex_archive" -C "$scratch_root" codex-aarch64-apple-darwin
  mv "$scratch_root/codex-aarch64-apple-darwin" "$darwin_root/codex"
  rm "$codex_archive"
  node "$repo/scripts/finalize-remote-darwin-manifest.mjs" "$resource_root"
fi

echo "==> Embedded remote payload"
du -sh "$resource_root"
for arch in amd64 arm64; do
  for name in vellum-remote-agent vellum-proxy-daemon codex; do
    test -s "$resource_root/linux-$arch/$name"
  done
done
test -s "$resource_root/manifest.json"
if [[ "$(uname -s)" == Darwin ]]; then
  for name in vellum-remote-agent vellum-proxy-daemon codex; do
    test -s "$resource_root/darwin-arm64/$name"
  done
fi

if [[ "$bundle" != stage-only ]]; then
  command -v pnpm >/dev/null || { echo "pnpm is required to bundle the desktop app" >&2; exit 1; }
  pnpm --dir "$repo" install --frozen-lockfile
  pnpm --dir "$repo" exec tauri build --bundles "$bundle"
fi
