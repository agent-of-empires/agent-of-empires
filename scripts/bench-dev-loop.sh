#!/usr/bin/env bash
# Times dev-loop rebuilds of the root crate (issue #5797 benchmark).
# Usage: scripts/bench-dev-loop.sh
# Benchmarks a copy of the tree it runs from, with a fresh one-commit git repo
# and its own debug target dir, both under a mktemp dir removed on exit. Needs
# node, npm and git; installs hyperfine with cargo if missing. Results stay in
# $TMPDIR/bench-dev-loop-<timestamp>/. Aborts below 10 GiB free disk.
set -euo pipefail
[ "${1:-}" = -h ] && { sed -n 3p "$0" | sed 's/^# //' >&2; exit 2; }

SRC=$(cd "$(dirname "$0")/.." && pwd)
TREE="synced tree"
[ -e "$SRC/.git" ] && TREE=$(git -C "$SRC" describe --always --dirty)
OUT=${TMPDIR:-/tmp}/bench-dev-loop-$(date +%Y%m%d-%H%M%S)
B=$(mktemp -d)
mkdir -p "$OUT"
trap 'rm -rf "$B"' EXIT
command -v hyperfine >/dev/null || cargo install hyperfine --locked -q

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
cargo test --no-run -q
"$DISK"

echo "== 1. build/test alternation"
hyperfine --runs 5 --export-markdown "$OUT/1-build-test.md" \
  --prepare "$DISK && touch src/lib.rs && cargo build -q" 'cargo test --no-run -q'
touch src/lib.rs && cargo build -q
log=$(cargo test --no-run -v 2>&1)
if grep -q 'kache [^ ]*rustc --crate-name' <<<"$log"; then echo "kache still in the build" >&2; exit 1; fi
echo "agent_of_empires rustc runs in cargo test --no-run after cargo build: $(count_lib_rustc <<<"$log")" |
  tee -a "$OUT/summary.txt"
for c in serde_json tokio tokio-stream regex-automata; do
  diff <(cargo tree -e normal -i "$c" -f '{p} {f}' --depth 0) \
    <(cargo tree -e all -i "$c" -f '{p} {f}' --depth 0) || true
done | tee "$OUT/feature-drift.diff"

echo "== 2. commit"
cargo build --tests -q
hyperfine --runs 5 --export-markdown "$OUT/2-commit.md" \
  -n commit --prepare "$DISK && git commit --allow-empty -qm bench" 'cargo build --tests -q' \
  -n control --prepare "$DISK" 'cargo build --tests -q'
git commit --allow-empty -qm bench
echo "agent_of_empires rustc runs in cargo build --tests after a commit: $(cargo build --tests -v 2>&1 | count_lib_rustc)" |
  tee -a "$OUT/summary.txt"

echo "== 3. xtask dev rebuild"
"$DISK"
cargo build --features web -q
hyperfine --runs 5 --export-markdown "$OUT/3-xtask-dev.md" \
  --prepare "$DISK && touch web/src/main.tsx src/lib.rs" \
  -n 'cargo build --features web' 'cargo build --features web -q' \
  -n 'cargo build' 'cargo build -q'

for f in "$OUT"/*; do echo "--- $(basename "$f")"; cat "$f"; done
echo "Results: $OUT"
