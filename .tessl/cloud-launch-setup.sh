#!/bin/bash
# Prepares Tessl cloud runs: https://docs.tessl.io/automations/cloud-setup-hook.md
# Only environments that set AOE_SETUP=full get the Rust and Node toolchain.
set -eo pipefail

mkdir -p "$HOME/.local/bin"

if [ "${AOE_SETUP:-}" != "full" ]; then
  echo "AOE_SETUP is not 'full': skipping toolchain setup"
  exit 0
fi

echo "Installing tmux, bubblewrap and iproute2"
if ! command -v tmux >/dev/null || ! command -v bwrap >/dev/null || ! command -v ip >/dev/null; then
  sudo apt-get update -qq
  sudo apt-get install -y -qq tmux bubblewrap iproute2
fi

echo "Installing rustup"
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --no-modify-path --profile minimal --default-toolchain none
fi

echo "Installing the toolchain pinned in rust-toolchain.toml"
"$HOME/.cargo/bin/rustup" toolchain install
ln -sf "$HOME"/.cargo/bin/* "$HOME/.local/bin/"

NEXTEST_VERSION=0.9.152
NEXTEST_SHA256=d039b43c3e0958441b6ad0a39c8884d018b3d2e8fb9928f209074b1100b5e17a
echo "Installing cargo-nextest $NEXTEST_VERSION"
if [ ! -x "$HOME/.local/bin/cargo-nextest" ]; then
  archive=$(mktemp)
  curl -LsSf -o "$archive" "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$NEXTEST_VERSION/cargo-nextest-$NEXTEST_VERSION-x86_64-unknown-linux-gnu.tar.gz"
  echo "$NEXTEST_SHA256  $archive" | sha256sum -c -
  tar zxf "$archive" -C "$HOME/.local/bin"
  rm -f "$archive"
fi

echo "Checking Node.js against the web lockfile's engines"
if ! node -e '
const [major, minor, patch] = process.versions.node.split(".").map(Number);
const ok = (major === 22 && (minor > 22 || (minor === 22 && patch >= 2))) ||
  (major === 24 && minor >= 15) || major >= 26;
process.exit(ok ? 0 : 1);'; then
  echo "Node.js ^22.22.2 || ^24.15.0 || >=26 is required, found $(node -v)"
  exit 1
fi

for dir in web acp-worker/test-shim; do
  echo "Installing npm dependencies in $dir"
  stamp="$dir/node_modules/.lockfile-sha256"
  sum=$(sha256sum "$dir/package-lock.json" | cut -d' ' -f1)
  if [ "$(cat "$stamp" 2>/dev/null)" != "$sum" ]; then
    (cd "$dir" && npm ci --no-audit --no-fund)
    echo "$sum" >"$stamp"
  fi
done
