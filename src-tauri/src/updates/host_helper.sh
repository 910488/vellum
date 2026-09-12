#!/bin/sh
# Host-side Vellum remote update helper. This is not the agent process:
# SSH drop, Vellum close, or agent self-replace must not abort it.
# Success is a restarted running set, not merely cp / docker load.
set -eu
ROOT="${1:?root}"
ACTION="${2:?apply-or-rollback}"
STEPS="$ROOT/steps"
PKG="$ROOT/package.tar.gz"
PREV="$ROOT/previous"
STAGE="$ROOT/stage"
LOCK="$ROOT/lock"
EXPECTED="$ROOT/expected.json"
RUNNING="$ROOT/running.json"

mkdir -p "$ROOT" "$PREV" "$STAGE"
touch "$STEPS"

already() {
  grep -qx "$1" "$STEPS" 2>/dev/null
}

record() {
  already "$1" && return 0
  printf '%s\n' "$1" >> "$STEPS"
}

package_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$PKG" | awk '{print $1}'
  else
    shasum -a 256 "$PKG" | awk '{print $1}'
  fi
}

verify_package() {
  [ -f "$PKG" ] || { echo "missing package.tar.gz" >&2; exit 1; }
  [ -f "$ROOT/sha256" ] || { echo "missing sha256" >&2; exit 1; }
  expected=$(tr -d -c '0-9a-fA-F' < "$ROOT/sha256")
  actual=$(package_sha256 | tr -d -c '0-9a-fA-F')
  [ "$actual" = "$expected" ] || {
    echo "package sha256 mismatch: $actual != $expected" >&2
    exit 1
  }
}

guard_tar_members() {
  # Do not pipe into while: exit 1 in a pipeline subshell would not
  # stop tar -xzf. List members to a file, then fail this process.
  list="$ROOT/tar-members.txt"
  tar -tzf "$PKG" >"$list"
  while IFS= read -r member || [ -n "$member" ]; do
    [ -z "$member" ] && continue
    case "$member" in
      /*|*..*|../*|*/../*|*/..)
        echo "illegal tar member $member" >&2
        exit 1
        ;;
    esac
  done <"$list"
}

proxy_image_from_expected() {
  [ -f "$EXPECTED" ] || return 0
  sed -n 's/.*"proxyImage"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p; s/.*"proxy_image"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$EXPECTED" | head -1
}

stop_running_set() {
  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop vellum-remote-agent vellum-remote-broker 2>/dev/null || true
  fi
  if command -v pkill >/dev/null 2>&1; then
    pkill -x vellum-remote-agent 2>/dev/null || true
    pkill -x vellum-remote-broker 2>/dev/null || true
  fi
  if command -v docker >/dev/null 2>&1; then
    ids=$(docker ps -aq --filter name=vellum-proxy 2>/dev/null || true)
    if [ -n "$ids" ]; then
      echo "$ids" | xargs docker stop >/dev/null 2>&1 || true
      echo "$ids" | xargs docker rm >/dev/null 2>&1 || true
    fi
  fi
}

start_running_set() {
  image=$(proxy_image_from_expected)
  if command -v docker >/dev/null 2>&1 && [ -n "${image:-}" ]; then
    docker run -d --name vellum-proxy --restart unless-stopped "$image" >/dev/null
  fi
  if [ -x "$HOME/.local/bin/vellum-remote-agent" ]; then
    nohup "$HOME/.local/bin/vellum-remote-agent" >>"$ROOT/agent.log" 2>&1 &
  fi
  if [ -x "$HOME/.local/bin/vellum-remote-broker" ]; then
    nohup "$HOME/.local/bin/vellum-remote-broker" >>"$ROOT/broker.log" 2>&1 &
  fi
}

restart_running_set() {
  stop_running_set
  start_running_set
}

write_running_json() {
  if [ -f "$EXPECTED" ]; then
    cp "$EXPECTED" "$RUNNING"
  fi
}

apply_locked() {
  if [ "$ACTION" = "rollback" ]; then
    if [ -d "$PREV" ]; then
      find "$PREV" -name 'vellum-remote-agent' -exec cp {} "$HOME/.local/bin/vellum-remote-agent" \;
      find "$PREV" -name 'vellum-remote-broker' -exec cp {} "$HOME/.local/bin/vellum-remote-broker" \;
      find "$PREV" -name 'proxy-image.tar' -exec docker load -i {} \; || true
      if [ -f "$PREV/expected.json" ]; then
        cp "$PREV/expected.json" "$EXPECTED"
      fi
    fi
    restart_running_set
    write_running_json
    record rollback
    exit 0
  fi

  already commit && exit 0

  if ! already stage; then
    verify_package
    guard_tar_members
    tar -xzf "$PKG" -C "$STAGE"
    record stage
  fi

  if ! already replaceSet; then
    for bin in vellum-remote-agent vellum-remote-broker; do
      if command -v "$bin" >/dev/null 2>&1; then
        cp "$(command -v "$bin")" "$PREV/" || true
      fi
    done
    if [ -f "$EXPECTED" ]; then
      cp "$EXPECTED" "$PREV/expected.json" || true
    fi
    find "$STAGE" -name 'vellum-remote-agent' -exec cp {} "$HOME/.local/bin/vellum-remote-agent" \;
    find "$STAGE" -name 'vellum-remote-broker' -exec cp {} "$HOME/.local/bin/vellum-remote-broker" \;
    find "$STAGE" -name 'proxy-image.tar' -exec docker load -i {} \;
    restart_running_set
    write_running_json
    record replaceSet
  fi
  record commit
}

if command -v flock >/dev/null 2>&1; then
  (
    flock 9
    apply_locked
  ) 9>"$LOCK"
else
  apply_locked
fi
