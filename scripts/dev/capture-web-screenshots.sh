#!/usr/bin/env bash
# Regenerate the web dashboard and cockpit documentation screenshots.
#
# Drives a seeded `aoe serve` (and a scripted fake ACP agent for the
# cockpit shots) through the live Playwright harness and writes hero PNGs
# into docs/assets/web/ and docs/assets/cockpit/. See
# docs/assets/web/README.md for the maintenance contract.
#
# Usage:
#   scripts/dev/capture-web-screenshots.sh
#
# Honors AOE_E2E_BINARY if set; otherwise prefers an existing
# target/release/aoe, then builds one with `--features serve`.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# Resolve a serve-enabled binary.
if [[ -n "${AOE_E2E_BINARY:-}" && -x "${AOE_E2E_BINARY}" ]]; then
  BIN="${AOE_E2E_BINARY}"
elif [[ -x "target/release/aoe" ]]; then
  BIN="target/release/aoe"
elif [[ -x "target/debug/aoe" ]]; then
  BIN="target/debug/aoe"
else
  echo "No aoe binary found; building with --features serve (this is slow on a cold cache)."
  cargo build --release --features serve
  BIN="target/release/aoe"
fi
export AOE_E2E_BINARY="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
echo "Using binary: $AOE_E2E_BINARY"

cd web
if [[ ! -d node_modules ]]; then
  echo "Installing web dependencies..."
  npm install
fi
# Chromium is required; install if the cache is empty (no-op when present).
# Silent when already cached; re-runs verbosely so a fresh install or error is visible.
npx playwright install chromium >/dev/null 2>&1 || npx playwright install chromium

echo "Capturing screenshots..."
npx playwright test --config=playwright.capture.config.ts "$@"

echo "Done. Review the diff under docs/assets/ before committing."
