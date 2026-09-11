# syntax=docker/dockerfile:1.7
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder
ARG TARGETPLATFORM
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
    --mount=type=cache,id=vellum-remote-target-${TARGETPLATFORM},target=/src/target,sharing=locked \
    cargo build --release -p vellum-remote-agent -p vellum-remote-broker \
    && mkdir -p /out \
    && cp target/release/vellum-remote-agent target/release/vellum-remote-broker /out/

FROM scratch AS export
COPY --from=builder /out/vellum-remote-agent /vellum-remote-agent
COPY --from=builder /out/vellum-remote-broker /vellum-remote-broker
