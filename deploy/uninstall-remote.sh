#!/usr/bin/env bash
set -euo pipefail

SYSTEMD_USER_DIR="${HOME}/.config/systemd/user"
BIN_DIR="${HOME}/.local/bin"

systemctl --user disable --now vellum-remote-broker.service 2>/dev/null || true
systemctl --user disable --now vellum-codex-app-server.service 2>/dev/null || true

rm -f "${SYSTEMD_USER_DIR}/vellum-remote-broker.service"
rm -f "${SYSTEMD_USER_DIR}/vellum-codex-app-server.service"
rm -f "${BIN_DIR}/vellum-remote-broker"

systemctl --user daemon-reload || true

echo "removed user services and broker binary"
echo "note: data under ~/.local/share/vellum was preserved"
