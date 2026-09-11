#!/usr/bin/env bash
# M0 proxy container smoke: install ? start ? version/health/readyz/models ? stop.
# Does NOT touch Codex config or inject Managed Profile.
set -euo pipefail

IMAGE="${1:-vellum-proxy:local}"
ROOT="${SMOKE_ROOT:-$(mktemp -d -t vellum-proxy-m0-XXXXXX)}"
PORT="${SMOKE_PORT:-15721}"
AGENT_BIN="${AGENT_BIN:-target/debug/vellum-remote-agent}"

echo "smoke root: $ROOT"
echo "image: $IMAGE"
echo "port: $PORT"

if [[ ! -x "$AGENT_BIN" ]]; then
  echo "building vellum-remote-agent..."
  cargo build -p vellum-remote-agent
  AGENT_BIN="target/debug/vellum-remote-agent"
fi

rpc() {
  local req="$1"
  printf '%s\n' "$req" | "$AGENT_BIN" --state-root "$ROOT" rpc
}

json_req() {
  # Prefer jq when available for safe JSON encoding of arbitrary image refs.
  if command -v jq >/dev/null 2>&1; then
    jq -cn "$@"
    return
  fi
  # Fallback: only used for fixed smoke payloads without untrusted interpolation.
  case "$1" in
    install)
      printf '{"method":"proxy.install","operationId":"smoke-install","image":"%s","imageDigest":null}\n' "$IMAGE"
      ;;
    start)
      printf '{"method":"proxy.start","operationId":"smoke-start","hostPort":%s,"image":null}\n' "$PORT"
      ;;
    stop)
      printf '{"method":"proxy.stop","operationId":"smoke-stop"}\n'
      ;;
    status)
      printf '{"method":"proxy.status"}\n'
      ;;
    *)
      echo "unknown fallback payload: $1" >&2
      exit 1
      ;;
  esac
}

echo "== install =="
if command -v jq >/dev/null 2>&1; then
  install_req="$(jq -cn --arg image "$IMAGE" '{method:"proxy.install",operationId:"smoke-install",image:$image,imageDigest:null}')"
else
  install_req="$(json_req install)"
fi
rpc "$install_req"

echo "== start =="
if command -v jq >/dev/null 2>&1; then
  start_req="$(jq -cn --argjson port "$PORT" '{method:"proxy.start",operationId:"smoke-start",hostPort:$port,image:null}')"
else
  start_req="$(json_req start)"
fi
rpc "$start_req"

base="http://127.0.0.1:${PORT}"
# Every endpoint, /health and /readyz included, requires the boundary key.
# Prefer the explicit env var; otherwise read the key the agent provisioned
# under this smoke root's secret store.
boundary_key="${VELLUM_PROXY_BOUNDARY_KEY:-}"
if [[ -z "$boundary_key" ]]; then
  secret="$ROOT/secrets/__vellum_proxy_boundary__"
  if [[ -f "$secret" ]]; then
    boundary_key="$(tr -d '\r\n' < "$secret")"
  fi
fi
if [[ -z "$boundary_key" ]]; then
  echo "missing VELLUM_PROXY_BOUNDARY_KEY and $ROOT/secrets/__vellum_proxy_boundary__" >&2
  exit 1
fi
header="x-vellum-boundary-key: ${boundary_key}"
echo "== probes =="
curl -fsS -H "$header" "$base/version" | tee "$ROOT/version.json"
curl -fsS -H "$header" "$base/health" | tee "$ROOT/health.json"
curl -fsS -H "$header" "$base/readyz" | tee "$ROOT/readyz.json"
curl -fsS -H "$header" "$base/v1/models" | tee "$ROOT/models.json"

echo "== stop =="
if command -v jq >/dev/null 2>&1; then
  stop_req="$(jq -cn '{method:"proxy.stop",operationId:"smoke-stop"}')"
else
  stop_req="$(json_req stop)"
fi
rpc "$stop_req"

echo "== absent check =="
if command -v jq >/dev/null 2>&1; then
  status_req="$(jq -cn '{method:"proxy.status"}')"
else
  status_req="$(json_req status)"
fi
status="$(rpc "$status_req")"
echo "$status" | tee "$ROOT/status-after-stop.json"
if echo "$status" | grep -q '"running":true'; then
  echo "expected running=false after stop" >&2
  exit 1
fi

echo "M0 proxy smoke OK (no Codex injection)"
