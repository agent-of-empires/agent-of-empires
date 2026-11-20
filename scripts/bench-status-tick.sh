#!/usr/bin/env bash
# Counts tmux execs per Daemon status tick (issue #5788 benchmark).
# Usage: scripts/bench-status-tick.sh <aoe-binary> [sessions=20] [seconds=120]
# Isolated: throwaway HOME and tmux socket. Hook files live under
# /tmp/aoe-hooks-$(id -u)/<instance id>/ and are removed on exit.
set -euo pipefail

AOE=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
N=${2:-20}
SECS=${3:-120}
REAL_TMUX=$(command -v tmux)
B=$(mktemp -d /tmp/aoe-bench.XXXXXX)
mkdir -p "$B/bin" "$B/home/proj"
cat > "$B/bin/tmux" <<SHIM
#!/bin/sh
printf '%s\n' "\$*" >> "$B/tmux.log"
exec "$REAL_TMUX" "\$@"
SHIM
chmod +x "$B/bin/tmux"

export HOME="$B/home" AOE_TMUX_SOCKET="$B/tmux.sock"
IDS=()
cleanup() {
  "$AOE" serve --stop >/dev/null 2>&1 || true
  "$REAL_TMUX" -S "$B/tmux.sock" kill-server 2>/dev/null || true
  for id in "${IDS[@]:-}"; do rm -rf "/tmp/aoe-hooks-$(id -u)/$id"; done
  rm -rf "$B"
}
trap cleanup EXIT

for i in $(seq 1 "$N"); do
  "$AOE" add "$HOME/proj" -t "bench-$i" --tool claude \
    --cmd-override "sh -c 'printf \"> \"; exec sleep 86400'" --launch </dev/null >/dev/null
done
# `add` may auto-start a Daemon without the shim.
"$AOE" serve --stop >/dev/null 2>&1 || true

while read -r id; do
  IDS+=("$id")
  d="/tmp/aoe-hooks-$(id -u)/$id"
  mkdir -p "$d" && chmod 700 "$d"
  echo idle > "$d/status"
done < <("$AOE" list --json | python3 -c 'import json,sys; [print(s["id"]) for s in json.load(sys.stdin)]')

PATH="$B/bin:$PATH" "$AOE" serve >"$B/serve.log" 2>&1 &
sleep 30
: > "$B/tmux.log"
sleep "$SECS"

ticks=$(grep -c '^list-panes -a' "$B/tmux.log" || true)
echo "sessions=$N window=${SECS}s ticks=$ticks"
awk '{print $1, $2}' "$B/tmux.log" | sort | uniq -c |
  awk -v t="$ticks" '{printf "%-28s total=%-6d per_tick=%.2f\n", $2" "$3, $1, (t ? $1/t : 0)}'
