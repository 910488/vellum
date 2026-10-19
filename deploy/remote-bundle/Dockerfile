# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.99.0
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder
ARG REMOTE_ARCH=amd64
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools python3-pip \
    && pip install --break-system-packages ziglang==0.13.0 \
    && cargo install --locked cargo-zigbuild \
    && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src-tauri ./src-tauri
COPY third_party/codex-0.150 ./third_party/codex-0.150
# Cache the registry and the target directory per target platform, so editing
# one crate does not recompile the dependency tree. The build products are
# copied out inside the same RUN because a cache mount is not part of the
# resulting layer and would be invisible to `COPY --from`.
RUN --mount=type=cache,id=vellum-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=vellum-remote-musl-${REMOTE_ARCH},target=/src/target,sharing=locked \
    case "$REMOTE_ARCH" in \
      amd64) target=x86_64-unknown-linux-musl; \
        if [ "$(uname -m)" = x86_64 ]; then \
          cargo build --target "$target" --release -p vellum-remote-agent -p vellum-proxy-daemon; \
        else \
          cargo zigbuild --target "$target" --release -p vellum-remote-agent -p vellum-proxy-daemon; \
        fi ;; \
      arm64) target=aarch64-unknown-linux-musl; cargo zigbuild --target "$target" --release -p vellum-remote-agent -p vellum-proxy-daemon ;; \
      *) echo "unsupported REMOTE_ARCH: $REMOTE_ARCH" >&2; exit 2 ;; \
    esac \
    && mkdir -p /out \
    && cp "target/$target/release/vellum-remote-agent" "target/$target/release/vellum-proxy-daemon" /out/

FROM scratch AS export
COPY --from=builder /out/vellum-remote-agent /vellum-remote-agent
COPY --from=builder /out/vellum-proxy-daemon /vellum-proxy-daemon
