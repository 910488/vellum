# Vellum Proxy Container

Multi-arch image for the headless model proxy.

## Build

```bash
# amd64
docker build -f deploy/proxy/Dockerfile -t vellum-proxy:local .

# arm64 (Jetson / aarch64)
docker buildx build --platform linux/arm64 -f deploy/proxy/Dockerfile -t vellum-proxy:arm64 --load .
```

Builder Rust version is parameterized (`ARG RUST_VERSION`, default current CI stable).

## Runtime contract

### Standalone image default
- Process user: non-root `65532:65532` (Dockerfile `USER`)

### Bind-mount deployment (remote agent)
- Process user: **host agent effective UID/GID** (`docker --user $(id -u):$(id -g)`)
- Host publish: `127.0.0.1:<port>:15721` only
- Required mounts:
  - `/etc/vellum` (ro) containing `proxy.toml` ? host dir mode `0700`, file `0600`
  - `/var/lib/vellum/data` ? host dir mode `0700`, host-owned
  - `/var/lib/vellum/history` ? same
  - `/var/log/vellum` ? same
  - `/run/secrets` (ro) ? host dir mode `0700`
- Forbidden mounts: `~/.codex`, docker.sock, SSH keys, arbitrary workspaces
- Labels managed by `vellum-remote-agent`:
  - `io.vellum.managed=true`
  - `io.vellum.component=proxy`
  - `io.vellum.host-id`
  - `io.vellum.install-id`
  - `io.vellum.config-hash`
  - `io.vellum.image-version`

## Image digest policy

RepoDigest and Image ID are **not** the same:

| Source | Meaning | Used for `docker run` |
|---|---|---|
| RepoDigest | registry manifest digest | `repo@sha256:...` |
| Image ID (`.Id`) | local config id | never as `repo@image-id` |

Production lifecycle:

1. `docker pull <ref>` (skipped for local-only tags such as `vellum-proxy:*` / `*:local`)
2. inspect RepoDigest + Image ID separately
3. compare expected **repo** digest when provided (fail closed on mismatch)
4. persist `imageDigest` only when RepoDigest exists
5. `docker run` prefers `repo@sha256:...`; local tags keep `vellum-proxy:local`

Do not rely on mutable `latest` for restore/reconcile.

Pulls performed while holding the host mutation lock are bounded by a 120s agent-side timeout so Desktop RPCs fail closed instead of hanging indefinitely.

## Readiness / M0 smoke (no Codex injection)

```bash
# after image build + agent install/start
curl -fsS http://127.0.0.1:15721/version
curl -fsS http://127.0.0.1:15721/health
curl -fsS http://127.0.0.1:15721/readyz
curl -fsS http://127.0.0.1:15721/v1/models
```

`GET /readyz` must return 200 with the expected `install_id` before Codex injection is allowed.

Helper script (host with docker + agent binary):

```bash
./scripts/smoke-proxy-m0.sh vellum-proxy:local
```

The smoke helper always passes `--state-root` and builds request JSON via `jq` when available.
