#!/usr/bin/env bash
set -euo pipefail

# Install Vellum Remote Broker + Codex app-server user services on Linux/Jetson.
# This script performs wiring only; it does not download untrusted binaries.

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.config/vellum"
SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
SHARE_DIR="${HOME}/.local/share/vellum"

mkdir -p "${BIN_DIR}" "${CONFIG_DIR}" "${SYSTEMD_USER_DIR}" \
  "${SHARE_DIR}/remote" "${SHARE_DIR}/codex-home" "${SHARE_DIR}/run" \
  "${SHARE_DIR}/remote/diagnostics"

if ! command -v codex >/dev/null 2>&1; then
  echo "error: codex CLI not found on PATH" >&2
  exit 1
fi

CODEX_STANDALONE="${HOME}/.codex/packages/standalone/current/codex"
if [[ ! -x "${CODEX_STANDALONE}" ]]; then
  echo "error: pinned codex binary missing or not executable: ${CODEX_STANDALONE}" >&2
  exit 1
fi

if command -v loginctl >/dev/null 2>&1; then
  loginctl enable-linger "${USER}" || true
fi

echo "codex version: $(codex --version || true)"
uname -m

if [[ -x "${ROOT_DIR}/target/release/vellum-remote-broker" ]]; then
  install -m 0755 "${ROOT_DIR}/target/release/vellum-remote-broker" \
    "${BIN_DIR}/vellum-remote-broker"
elif [[ -x "${ROOT_DIR}/target/debug/vellum-remote-broker" ]]; then
  install -m 0755 "${ROOT_DIR}/target/debug/vellum-remote-broker" \
    "${BIN_DIR}/vellum-remote-broker"
else
  echo "error: build vellum-remote-broker first (cargo build -p vellum-remote-broker)" >&2
  exit 1
fi

if [[ ! -f "${CONFIG_DIR}/remote.toml" ]]; then
  cat > "${CONFIG_DIR}/remote.toml" <<EOF
broker_id = "$(hostname)-vellum"
data_dir = "${SHARE_DIR}/remote"
listen_addr = "127.0.0.1:45100"
codex_binary = "${HOME}/.codex/packages/standalone/current/codex"
codex_home = "${SHARE_DIR}/codex-home"
app_server_socket = "${SHARE_DIR}/run/codex-app-server.sock"
allowed_versions = ["0.146.1"]
allowed_roots = ["${HOME}/projects"]
require_auth = false
local_only = true
writer_lease_ttl_secs = 30
writer_lease_heartbeat_secs = 10
writer_lease_disconnect_grace_secs = 15
EOF
fi

install -m 0644 "${ROOT_DIR}/deploy/systemd/vellum-codex-app-server.service" \
  "${SYSTEMD_USER_DIR}/vellum-codex-app-server.service"
install -m 0644 "${ROOT_DIR}/deploy/systemd/vellum-remote-broker.service" \
  "${SYSTEMD_USER_DIR}/vellum-remote-broker.service"

systemctl --user daemon-reload
systemctl --user enable --now vellum-codex-app-server.service
systemctl --user enable --now vellum-remote-broker.service

echo "installed:"
echo "  ${BIN_DIR}/vellum-remote-broker"
echo "  ${CONFIG_DIR}/remote.toml"
echo "  user services: vellum-codex-app-server, vellum-remote-broker"
echo "downstream default: ws://127.0.0.1:45100/ws (SSH tunnel recommended)"
