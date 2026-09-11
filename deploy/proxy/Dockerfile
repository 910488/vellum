# syntax=docker/dockerfile:1.7

# Keep in sync with CI/local toolchain (workspace rust-version is a floor; image
# builds need a current stable for lockfile crates such as ICU/IDNA).
ARG RUST_VERSION=1.97.1
FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY src-tauri ./src-tauri
COPY third_party/codex-0.150 ./third_party/codex-0.150
RUN cargo build -p vellum-proxy-daemon --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --create-home --home-dir /var/lib/vellum vellum \
    && mkdir -p /var/lib/vellum/data /var/lib/vellum/history /var/log/vellum /etc/vellum /run/secrets \
    && chown -R 65532:65532 /var/lib/vellum /var/log/vellum \
    && chmod 0755 /var/lib/vellum /var/lib/vellum/data /var/lib/vellum/history /var/log/vellum \
    && chmod 0755 /etc/vellum /run/secrets
COPY --from=builder /src/target/release/vellum-proxy-daemon /usr/local/bin/vellum-proxy-daemon
# Image default for standalone runs. Remote-agent bind-mount deployments override
# --user with the host agent effective UID/GID so host-private mounts work.
USER 65532:65532
EXPOSE 15721
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=12 \
  CMD vellum-proxy-daemon doctor --config /etc/vellum/proxy.toml || exit 1
ENTRYPOINT ["vellum-proxy-daemon"]
CMD ["serve", "--config", "/etc/vellum/proxy.toml", "--listen", "0.0.0.0:15721"]
