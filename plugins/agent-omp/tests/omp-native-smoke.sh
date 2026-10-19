#!/bin/sh
set -eu

omp_bin=${OMP_BIN:-omp}
forced=${OMP_SMOKE_FORCED:-0}
set -- --approval-mode always-ask
if [ "$forced" = 1 ]; then set -- --auto-approve; fi
plugin_dir=$(CDPATH= cd -- "$(dirname -- "$0")/../omp-extension" && pwd)
smoke_extension=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/omp-native-smoke-extension.ts
tmp_home=$(mktemp -d "${TMPDIR:-/tmp}/omp-integration-smoke.XXXXXX")
uid=$(id -u)
instance_id="omp_smoke_$(date +%s)_$$"
hook_root="/tmp/aoe-hooks-$uid"
hook_dir="$hook_root/$instance_id"
marker="$tmp_home/session-started"
log="$tmp_home/omp.log"
resume_file="$tmp_home/resume.jsonl"
expected_id=""
mkdir -p "$tmp_home/tmp"

cleanup() {
  rm -f "$hook_dir/integration-omp.agent-of-empires.json" "$hook_dir/integration-omp.agent-of-empires.reply.json" "$hook_dir/integration-omp.agent-of-empires.control.json"
  rmdir "$hook_dir" 2>/dev/null || true
  rmdir "$hook_root" 2>/dev/null || true
  rm -rf "$tmp_home"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

for phase in initial resumed; do
  phase_forced="$forced"
  if [ "$phase" = resumed ]; then
    set -- --resume "$resume_file" --approval-mode always-ask
    phase_forced=0
    rm -f "$marker"
  fi
if env -i \
  PATH="$PATH" \
  HOME="$tmp_home" \
  TMPDIR="$tmp_home/tmp" \
  XDG_CONFIG_HOME="$tmp_home/config" \
  XDG_DATA_HOME="$tmp_home/data" \
  XDG_CACHE_HOME="$tmp_home/cache" \
  AOE_INSTANCE_ID="$instance_id" \
  OMP_SMOKE_MARKER="$marker" \
  OMP_SMOKE_FORCED="$phase_forced" \
  OMP_SMOKE_EXPECTED_ID="$expected_id" \
  OMP_SMOKE_RESUME_FILE="$resume_file" \
  "$omp_bin" "$@" --no-extensions --no-rules --no-skills --model openai/gpt-5.2 --extension "$plugin_dir/index.ts" --extension "$smoke_extension" --print "native extension smoke" >"$log" 2>&1; then
  :
elif [ ! -f "$marker" ]; then
  cat "$log" >&2
  printf '%s\n' "OMP did not reach session_start; ensure OMP_BIN names a compatible OMP CLI." >&2
  exit 1
fi

[ "$(cat "$marker" 2>/dev/null || true)" = "integration-snapshot-ok" ] || {
  cat "$log" >&2
  printf '%s\n' "OMP native approval control smoke failed: $(cat "$marker" 2>/dev/null || true)" >&2
  exit 1
}
expected_id=$(cat "$marker.sid")
done
