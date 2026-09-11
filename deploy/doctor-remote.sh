#!/usr/bin/env bash
set -euo pipefail

echo "== Vellum Remote Doctor =="
echo "host: $(hostname)"
echo "arch: $(uname -m)"
echo "date: $(date -Is)"
echo

echo "-- codex --"
if command -v codex >/dev/null 2>&1; then
  codex --version || true
else
  echo "codex not found"
fi
echo

echo "-- broker binary --"
if [[ -x "${HOME}/.local/bin/vellum-remote-broker" ]]; then
  ls -l "${HOME}/.local/bin/vellum-remote-broker"
else
  echo "vellum-remote-broker not installed"
fi
echo

echo "-- systemd user units --"
systemctl --user status vellum-codex-app-server.service --no-pager || true
systemctl --user status vellum-remote-broker.service --no-pager || true
echo

echo "-- sockets / ports --"
SOCKET="${HOME}/.local/share/vellum/run/codex-app-server.sock"
if [[ -S "${SOCKET}" ]]; then
  ls -l "${SOCKET}"
else
  echo "app-server socket missing: ${SOCKET}"
fi
ss -ltn | grep 45100 || echo "loopback :45100 not listening"
echo

echo "-- health endpoints --"
curl -fsS "http://127.0.0.1:45100/healthz" || echo "healthz failed"
echo
curl -fsS "http://127.0.0.1:45100/readyz" || echo "readyz failed"
echo
curl -fsS "http://127.0.0.1:45100/version" || echo "version failed"
echo

echo "-- process tree (sanitized) --"
ps -ef | grep -E 'vellum-remote-broker|codex app-server' | grep -v grep || true
echo

echo "doctor complete"
