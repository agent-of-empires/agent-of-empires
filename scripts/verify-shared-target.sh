#!/usr/bin/env bash
set -euo pipefail

# verify-shared-target.sh: prove that the committed kache rustc wrapper shares
# (and deduplicates) dependency build artifacts across separate worktree target
# dirs. See docs/development.md, "Faster rebuilds across worktrees (kache)".
#
# Why kache's own counters and not stat(1)? kache materializes a cached artifact
# into a target dir with the cheapest mechanism the filesystem supports: a
# reflink (copy-on-write clone) on APFS/btrfs/xfs, a hardlink on ext4, and only
# a full copy as a last resort. A reflink shares the underlying disk blocks but
# is a distinct inode with link count 1, so it is indistinguishable from a plain
# copy under stat(1). An inode/link-count probe therefore reports "not shared"
# on macOS (APFS) and most modern Linux filesystems even when dedup is perfect.
# kache records exactly what it did per restore (reflinked / hardlinked / copied
# bytes), so we assert on those instead; the check is correct on every platform.
#
# Two modes:
#
#   --self-test   No cargo, no kache. Feeds a known-good and a known-bad storage
#                 report through the same threshold checker the full run uses and
#                 asserts it accepts the first and rejects the second. Runs in CI
#                 in seconds, guards the checker logic against bit-rot.
#
#   (no args)     Full proof. Builds this workspace twice into two separate
#                 CARGO_TARGET_DIRs through kache against an isolated temporary
#                 store, then reads kache's restore counters and asserts the
#                 second (warm) build restored its dependency artifacts as
#                 reflinks/hardlinks (shared blocks) rather than copies. Also
#                 builds --no-default-features into a third dir to prove serve and
#                 non-serve builds coexist against one store. Needs kache
#                 installed and a single filesystem under $TMPDIR; skips cleanly
#                 otherwise.
#
# Usage:
#   scripts/verify-shared-target.sh --self-test
#   scripts/verify-shared-target.sh

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# A warm restore must be at least this fraction zero-copy (reflinked or
# hardlinked rather than copied) for us to trust that kache is sharing blocks.
# Real runs report 100%; the margin tolerates a stray non-cacheable artifact.
MIN_ZERO_COPY_PCT=90

info() { printf '  %s\n' "$*"; }
ok() { printf '  \033[32mok\033[0m %s\n' "$*"; }
fail() {
  printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2
  exit 1
}
skip() {
  printf '  \033[33mskip\033[0m %s\n' "$*"
  exit 0
}

# Decide whether a kache report's `storage` block proves zero-copy sharing.
# Args: <report.json path> <min zero-copy pct>. Exits 0 (shared) or 1 (not
# shared); prints a one-line human summary on stderr either way. The JSON comes
# in as a file path, not stdin, so the heredoc that carries the Python program
# does not collide with the data. Shared by both modes so the self-test
# exercises the exact logic the full run depends on.
check_storage() {
  local report="$1" min_pct="$2"
  python3 - "$report" "$min_pct" <<'PY'
import json, sys

report_path, min_pct = sys.argv[1], float(sys.argv[2])
try:
    with open(report_path) as fh:
        storage = json.load(fh).get("storage", {})
except (OSError, ValueError, AttributeError) as exc:
    print(f"could not parse kache report JSON: {exc}", file=sys.stderr)
    sys.exit(1)

reflinked = int(storage.get("reflinked_bytes", 0))
hardlinked = int(storage.get("hardlinked_bytes", 0))
copied = int(storage.get("copied_bytes", 0))
restored = int(storage.get("restored_bytes", 0))
shared = reflinked + hardlinked

if restored <= 0:
    print("no bytes were restored from the cache (no warm hits to measure)", file=sys.stderr)
    sys.exit(1)

# zero_copy_pct is reported directly; fall back to computing it if absent.
pct = storage.get("zero_copy_pct")
pct = float(pct) if pct is not None else shared / restored * 100.0

summary = (f"restored {restored} B: reflinked {reflinked}, hardlinked {hardlinked}, "
           f"copied {copied} ({pct:.1f}% zero-copy)")
if shared > 0 and copied * 10 <= shared and pct >= min_pct:
    print(summary, file=sys.stderr)
    sys.exit(0)
print(summary + " -> below threshold", file=sys.stderr)
sys.exit(1)
PY
}

self_test() {
  echo "verify-shared-target.sh --self-test"
  command -v python3 >/dev/null 2>&1 ||
    skip "python3 not found; needed to parse kache reports."

  local dir
  dir="$(mktemp -d)"
  trap 'rm -rf "$dir"' EXIT

  # A warm reflinking restore: everything shared, nothing copied.
  printf '%s' '{"storage":{"reflinked_bytes":3325348612,"hardlinked_bytes":0,"copied_bytes":0,"restored_bytes":3325348612,"zero_copy_pct":100.0}}' >"$dir/good.json"
  # A degraded restore: kache fell back to copying every artifact.
  printf '%s' '{"storage":{"reflinked_bytes":0,"hardlinked_bytes":0,"copied_bytes":3325348612,"restored_bytes":3325348612,"zero_copy_pct":0.0}}' >"$dir/bad.json"

  check_storage "$dir/good.json" "$MIN_ZERO_COPY_PCT" 2>/dev/null ||
    fail "checker rejected a fully reflinked report"
  ok "checker accepts a zero-copy (reflinked) restore"

  if check_storage "$dir/bad.json" "$MIN_ZERO_COPY_PCT" 2>/dev/null; then
    fail "checker accepted an all-copies report"
  fi
  ok "checker rejects an all-copies restore"

  trap - EXIT
  rm -rf "$dir"
  echo "self-test passed."
}

full_run() {
  echo "verify-shared-target.sh (full)"

  command -v kache >/dev/null 2>&1 ||
    skip "kache not installed; see docs/development.md to install it."
  command -v cargo >/dev/null 2>&1 ||
    skip "cargo not found on PATH."
  command -v python3 >/dev/null 2>&1 ||
    skip "python3 not found; needed to parse kache reports."

  local base
  base="$(mktemp -d)"
  trap 'rm -rf "$base"' EXIT

  # Isolate the kache store under the same temp tree as the target dirs so the
  # run is deterministic, leaves the real cache untouched, keeps store and
  # targets on one filesystem (reflinks/hardlinks cannot span filesystems), and
  # scopes the event log we read back to exactly these builds.
  export KACHE_CACHE_DIR="$base/store"
  export RUSTC_WRAPPER=kache
  export CARGO_INCREMENTAL=0
  mkdir -p "$KACHE_CACHE_DIR"

  local target_a="$base/target-a" target_b="$base/target-b" target_serve="$base/target-serve"

  info "building workspace into target-a (cold, populates the kache store)..."
  cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --target-dir "$target_a" >/dev/null

  info "building the same workspace into target-b (a second worktree, warm)..."
  cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --target-dir "$target_b" >/dev/null

  [ -d "$target_b/debug/deps" ] ||
    fail "expected $target_b/debug/deps to exist after building"

  info "asking kache how it materialized the warm restore..."
  kache report --format json --since 1h >"$base/report.json"
  if check_storage "$base/report.json" "$MIN_ZERO_COPY_PCT"; then
    ok "warm restore shared dependency blocks across target dirs (reflink/hardlink, no copy)"
  else
    fail "kache copied dependency artifacts instead of sharing them; no disk dedup across worktrees"
  fi

  info "building --no-default-features into a third dir (serve + non-serve coexist)..."
  cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --no-default-features --target-dir "$target_serve" >/dev/null ||
    fail "--no-default-features build failed against the shared store"
  ok "--no-default-features builds against the same store as the default (serve) builds"

  trap - EXIT
  rm -rf "$base"
  echo "full verification passed: dependency artifacts are shared and deduplicated across worktrees."
}

case "${1:-}" in
  --self-test) self_test ;;
  "") full_run ;;
  *)
    echo "unknown argument: $1" >&2
    echo "usage: $0 [--self-test]" >&2
    exit 2
    ;;
esac
