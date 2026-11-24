#!/usr/bin/env bash
# Counts tmux execs per Daemon status tick (issue #5788 benchmark).
# Usage: scripts/bench-status-tick.sh <aoe-binary> [sessions=20] [seconds=120]
# Isolated: throwaway HOME and tmux socket. Sessions are created over the
# Daemon's HTTP API, since a CLI-spawned Daemon cannot confirm startup where
# `ps -E` hides process environments (macOS). Hook files live under
# /tmp/aoe-hooks-$(id -u)/<instance id>/ and are removed on exit.
set -euo pipefail
[ $# -ge 1 ] || { sed -n 3p "$0" | sed 's/^# //' >&2; exit 2; }

AOE=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
N=${2:-20}
SECS=${3:-120}
PORT=${AOE_BENCH_PORT:-18788}
REAL_TMUX=$(command -v tmux)
B=$(mktemp -d "$HOME/.aoe-bench.XXXXXX")
mkdir -p "$B/bin" "$B/home/proj"
cat > "$B/bin/tmux" <<SHIM
#!/bin/sh
printf '%s\n' "\$*" >> "$B/tmux.log"
exec "$REAL_TMUX" "\$@"
SHIM
chmod +x "$B/bin/tmux"

export HOME="$B/home" AOE_TMUX_SOCKET="$B/tmux.sock"
# An inherited absolute XDG path would beat the throwaway HOME.
export XDG_CONFIG_HOME="$B/home/.config" XDG_DATA_HOME="$B/home/.local/share" \
  XDG_STATE_HOME="$B/home/.local/state" XDG_CACHE_HOME="$B/home/.cache"
# Agent config-dir overrides would also beat it, so hook installs stay in $B.
unset CLAUDE_CONFIG_DIR CODEX_HOME
IDS=()
SERVE_PID=  # never inherit the caller's value
cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null || true
  "$REAL_TMUX" -S "$B/tmux.sock" kill-server 2>/dev/null || true
  for id in ${IDS[@]+"${IDS[@]}"}; do
    [ -n "$id" ] && rm -rf "/tmp/aoe-hooks-$(id -u)/$id"
  done
  rm -rf "$B"
}
trap cleanup EXIT

"$AOE" hooks approve >/dev/null  # the Daemon refuses host sessions until approved
PATH="$B/bin:$PATH" "$AOE" serve --host 127.0.0.1 --port "$PORT" --no-autostart \
  >"$B/serve.log" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 60); do
  grep -q 'token=' "$B/serve.log" && break
  kill -0 "$SERVE_PID" 2>/dev/null || break
  sleep 1
done
if ! grep -q 'token=' "$B/serve.log"; then
  echo "Daemon did not start:" >&2
  cat "$B/serve.log" >&2
  exit 1
fi
TOKEN=$(grep -o 'token=[0-9a-f]*' "$B/serve.log" | head -1 | cut -d= -f2)
API="http://127.0.0.1:$PORT/api/sessions"
AUTH="Authorization: Bearer $TOKEN"

for i in $(seq 1 "$N"); do
  curl -fsS -X POST "$API" -H "$AUTH" -H 'content-type: application/json' -d "{
    \"title\":\"bench-$i\",\"path\":\"$HOME/proj\",\"tool\":\"claude\",\"view\":\"terminal\",
    \"command_override\":\"sh -c 'printf \\\"> \\\"; exec sleep 86400'\"}" >/dev/null
done

LIST=$(curl -fsS "$API" -H "$AUTH" |
  python3 -c 'import json,sys; [print(s["id"]) for s in json.load(sys.stdin)["sessions"]]')
[ "$(printf '%s\n' "$LIST" | grep -c .)" -eq "$N" ] || {
  echo "expected $N sessions, got: $LIST" >&2
  exit 1
}
# Write hook files only under a hook dir this user owns and keeps private, so no other
# user can swap a child for a symlink.
H="/tmp/aoe-hooks-$(id -u)"
mkdir -p -m 700 "$H"
[ -d "$H" ] && [ ! -L "$H" ] && [ -O "$H" ] || {
  echo "$H is not a directory owned by $(id -un)" >&2
  exit 1
}
chmod 700 "$H"
while read -r id; do
  IDS+=("$id")
  d="$H/$id"
  mkdir -p "$d" && chmod 700 "$d"
  echo idle > "$d/status"
done <<< "$LIST"

sleep 30
: > "$B/tmux.log"
sleep "$SECS"

sed -i.bak -E 's/^(-u |-S [^ ]+ )+//' "$B/tmux.log"
ticks=$(grep -c '^list-panes -a' "$B/tmux.log" || true)
echo "sessions=${#IDS[@]} window=${SECS}s ticks=$ticks"
# One exec can chain commands with `;`: count it by its capture, not its first command.
awk '/ ; capture-pane / {print "capture-pane", "(chained)"; next} {print $1, $2}' "$B/tmux.log" | sort | uniq -c |
  awk -v t="$ticks" '{printf "%-28s total=%-6d per_tick=%.2f\n", $2" "$3, $1, (t ? $1/t : 0)}'
