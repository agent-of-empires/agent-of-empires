# Installation

**Prerequisites:** [tmux](https://github.com/tmux/tmux/wiki). Docker (or another container runtime) is optional, for [sandboxing](guides/sandbox.md), and [Node.js](https://nodejs.org/) is needed only to build the web dashboard from source. Building also needs a C toolchain for the bundled native dependencies (SQLite, libgit2, OpenSSL, liblzma, AWS-LC); a stock `cc` covers most platforms, and targets without pre-generated AWS-LC bindings also need CMake.

## Install

```bash
# Quick install (Linux and macOS)
curl -fsSL \
  https://raw.githubusercontent.com/agent-of-empires/agent-of-empires/main/scripts/install.sh \
  | bash

# Homebrew
brew install aoe

# From source; add --features web for the dashboard (needs Node and npm)
git clone https://github.com/agent-of-empires/agent-of-empires
cd agent-of-empires && cargo build --release
```

A source build leaves the binary at `target/release/aoe`. Verify any install with `aoe --version`.

## Updating

```bash
aoe update
```

`aoe update` detects how aoe was installed (Homebrew, the install script, Nix, or Cargo) and dispatches to the right mechanism. For Nix and Cargo it prints the manual command instead, since those need external tooling. In the TUI, press `u` while the update bar is visible to run the same flow, or `Ctrl+x` to dismiss the bar.

If you installed shell completions as a static file, regenerate it afterwards so it picks up new commands and flags; see [Shell completions](guides/shell-completions.md) for the always-fresh setup that avoids this.

## Uninstalling

```bash
aoe uninstall
```

It prompts before removing the binary, the app data directory, and the tmux settings.
