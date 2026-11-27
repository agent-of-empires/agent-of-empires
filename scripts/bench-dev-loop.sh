#!/usr/bin/env bash
# Times dev-loop rebuilds of the root crate (issue #5797 benchmark).
# Usage: scripts/bench-dev-loop.sh [scenario number...] (default: all)
# Benchmarks a copy of the tree it runs from, with a fresh one-commit git repo
# and its own debug target dir, both under a mktemp dir removed on exit. Needs
# node, npm, git and jq; installs hyperfine with cargo if missing. Results stay
# in $TMPDIR/bench-dev-loop-<timestamp>-*/. Aborts below 10 GiB free disk.
set -euo pipefail
[ "${1:-}" = -h ] && { sed -n 3p "$0" | sed 's/^# //' >&2; exit 2; }

unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE
SRC=$(cd "$(dirname "$0")/.." && pwd)
TREE="synced tree"
[ -e "$SRC/.git" ] && TREE=$(git -C "$SRC" describe --always --dirty)
OUT=$(mktemp -d "${TMPDIR:-/tmp}/bench-dev-loop-$(date +%Y%m%d-%H%M%S)-XXXX")
B=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$B"' EXIT
RUN=" ${*:-1 2 3} "
command -v hyperfine >/dev/null || cargo install hyperfine --locked -q
command -v jq >/dev/null || { echo "needs jq" >&2; exit 2; }

# Bypass kache: real cargo (shim dir off PATH), no rustc wrapper, plain rustdoc and host CC.
PATH=$(printf %s "$PATH" | tr : '\n' | grep -v '/kache/shims$' | paste -sd: -)
export PATH RUSTC_WRAPPER='' RUSTDOC=rustdoc HOST_CC=cc HOST_CXX=c++
export CARGO_TARGET_DIR="$B/target"
export GIT_AUTHOR_NAME=bench GIT_AUTHOR_EMAIL=bench@localhost \
  GIT_COMMITTER_NAME=bench GIT_COMMITTER_EMAIL=bench@localhost GIT_CONFIG_GLOBAL=/dev/null
unset AOE_WEB_DIST

cat > "$B/disk-ok" <<EOF
#!/bin/sh
[ "\$(df -k "$B" | awk 'NR==2 {print \$4}')" -ge $((10 * 1024 * 1024)) ] && exit 0
echo "under 10 GiB free disk, aborting" >&2
exit 1
EOF
chmod +x "$B/disk-ok"
DISK="$B/disk-ok"

count_lib_rustc() { grep -c 'Running .*--crate-name agent_of_empires' || true; }
target_mib() { du -sm "$CARGO_TARGET_DIR" | cut -f1; }

# Runs "$@" in the foreground and sets PEAK_MIB to the peak summed RSS of rustc processes
# writing to this bench's target dir.
with_rustc_rss() {
  echo 0 >"$B/rss"
  (p=0; while sleep 0.1; do
    s=$(ps -ewwo rss=,args= | awk -v t="$CARGO_TARGET_DIR" '$2 ~ /(^|\/)rustc$/ && index($0, t) {s += $1} END {print s + 0}')
    [ "$s" -gt "$p" ] && p=$s && echo "$p" >"$B/rss"
  done) &
  local sampler=$!
  "$@"
  kill "$sampler"
  wait "$sampler" 2>/dev/null || true
  PEAK_MIB=$(($(cat "$B/rss") / 1024))
}

# .git is never copied: in a worktree it points at the real repo, so bench commits would move its branch.
rsync -a --exclude=/.git --exclude=/target --exclude=/web/node_modules --exclude=/web/dist "$SRC"/ "$B/src"/
cd "$B/src"
git init -q && git add -A && git commit -qm base
{
  echo "tree=$TREE $(uname -m) nproc=$(nproc 2>/dev/null || sysctl -n hw.ncpu) jobs=${CARGO_BUILD_JOBS:-config}"
  echo "$(rustc -V); $(cargo -V); load:$(uptime | sed 's/.*load average[s]*://')"
} | tee "$OUT/summary.txt"
(cd web && npm ci --no-audit --no-fund --silent)

"$DISK"
cargo build -q
before=$(target_mib)
cargo test --no-run -q
echo "target dir growth from the first cargo test --no-run after cargo build: $(($(target_mib) - before)) MiB" |
  tee -a "$OUT/summary.txt"
"$DISK"

if [[ $RUN == *" 1 "* ]]; then
echo "== 1. build/test alternation"
hyperfine --runs 5 --export-markdown "$OUT/1-build-test.md" --export-json "$OUT/1-build-test.json" \
  --prepare "\"$DISK\" && touch src/lib.rs && cargo build -q" 'cargo test --no-run -q'
echo "cargo test --no-run mean CPU seconds: $(jq -r '.results[0] | "user \(.summary.time_user.mean // .user) system \(.summary.time_system.mean // .system)"' "$OUT/1-build-test.json")" |
  tee -a "$OUT/summary.txt"
touch src/lib.rs && cargo build -q
with_rustc_rss cargo test --no-run -v >"$B/log" 2>&1
log=$(cat "$B/log")
if grep -q 'kache [^ ]*rustc --crate-name' <<<"$log"; then echo "kache still in the build" >&2; exit 1; fi
echo "agent_of_empires rustc runs in cargo test --no-run after cargo build: $(count_lib_rustc <<<"$log")" |
  tee -a "$OUT/summary.txt"
echo "peak summed rustc RSS (sampled every 0.1 s) in that cargo test --no-run: $PEAK_MIB MiB" | tee -a "$OUT/summary.txt"
for c in serde_json tokio tokio-stream regex-automata; do
  diff <(cargo tree -e normal -i "$c" -f '{p} {f}' --depth 0) \
    <(cargo tree -e all -i "$c" -f '{p} {f}' --depth 0) || true
done | tee "$OUT/feature-drift.diff"
fi

if [[ $RUN == *" 2 "* ]]; then
echo "== 2. commit"
cargo build --tests -q
hyperfine --runs 5 --export-markdown "$OUT/2-commit.md" \
  -n commit --prepare "\"$DISK\" && git commit --allow-empty -qm bench" 'cargo build --tests -q' \
  -n control --prepare "\"$DISK\"" 'cargo build --tests -q'
git commit --allow-empty -qm bench
echo "agent_of_empires rustc runs in cargo build --tests after a commit: $(cargo build --tests -v 2>&1 | count_lib_rustc)" |
  tee -a "$OUT/summary.txt"
fi

if [[ $RUN == *" 3 "* ]]; then
echo "== 3. xtask dev rebuild"
"$DISK"
cargo build --features web -q
mkdir -p "$B/empty-dist"
# Toggling AOE_WEB_DIST reruns build.rs, so each row gets a warm-up run.
hyperfine --runs 5 --warmup 1 --export-markdown "$OUT/3-xtask-dev.md" \
  -n 'web, Rust edit' --prepare "\"$DISK\" && touch src/lib.rs" 'cargo build --features web -q' \
  -n 'web, Rust edit after a frontend edit' --prepare "\"$DISK\" && touch web/src/main.tsx src/lib.rs" 'cargo build --features web -q' \
  -n 'web, empty bundle, Rust edit after a frontend edit' --prepare "\"$DISK\" && touch web/src/main.tsx src/lib.rs" \
  "env AOE_WEB_DIST=\"$B/empty-dist\" cargo build --features web -q"
fi

for f in "$OUT"/*; do echo "--- $(basename "$f")"; cat "$f"; done
echo "Results: $OUT"
