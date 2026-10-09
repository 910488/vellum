#!/usr/bin/env bash
# Warn when the commit being built does not contain origin/main.
#
# A build from a feature branch that forked before a fix landed on main ships
# without that fix, and nothing in the installer says so. That is how a macOS
# build once resolved Codex through a Homebrew `#!/usr/bin/env node` wrapper
# after main had already stopped doing that. The check never fails the build
# unless VELLUM_REQUIRE_MAIN=1; it makes the gap visible.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
git_in_repo() { git -C "$repo" "$@"; }

git_in_repo rev-parse --git-dir >/dev/null 2>&1 || exit 0
git_in_repo fetch --quiet --no-tags origin \
  "+refs/heads/main:refs/remotes/origin/main" >/dev/null 2>&1 || true
git_in_repo rev-parse --verify --quiet origin/main >/dev/null || {
  echo "check-main-ancestry: origin/main is unavailable; skipped" >&2
  exit 0
}

if git_in_repo merge-base --is-ancestor origin/main HEAD 2>/dev/null; then
  exit 0
fi

# A shallow clone cannot answer the question; say so instead of a false alarm.
if [[ "$(git_in_repo rev-parse --is-shallow-repository 2>/dev/null)" == "true" ]]; then
  echo "check-main-ancestry: shallow clone; fetch with full history to check" >&2
  exit 0
fi

head="$(git_in_repo rev-parse --short HEAD)"
branch="$(git_in_repo rev-parse --abbrev-ref HEAD)"
missing="$(git_in_repo rev-list --count HEAD..origin/main)"
message="This build ($branch @ $head) is missing $missing commit(s) from origin/main; fixes landed on main are not in it."

echo "WARNING: $message" >&2
git_in_repo log --oneline --no-decorate -n 10 HEAD..origin/main >&2 || true
if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
  echo "::warning title=Build is behind main::$message"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
      echo "### ⚠️ Build is behind main"
      echo
      echo "$message"
      echo
      echo '```'
      git_in_repo log --oneline --no-decorate -n 20 HEAD..origin/main
      echo '```'
    } >>"$GITHUB_STEP_SUMMARY"
  fi
fi
[[ "${VELLUM_REQUIRE_MAIN:-0}" == "1" ]] && exit 1
exit 0
