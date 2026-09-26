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

## Downgrading

Downgrades are not supported: the older build refuses to start when `.schema_version` records a newer data schema, and that refusal is the safe outcome, because a migration that retypes a persisted field writes a shape the previous release cannot read.

### Finding out whether an older build can read your data

1. Copy every `profiles/<name>/sessions.json` and the `groups.json` beside it out of the [app directory](guides/configuration.md#file-locations), plus the top-level `sessions.json` if the pre-profiles layout left one. A retype migration rewrites no other file; `groups.json` comes along so a return trip restores a matching pair.
2. Run the older build once and read the number it prints when it refuses, its own schema version. Then copy `.schema_version` as a record and edit it in place down to that number. Deleting it is not the safer variant: a missing file reads as version 0, so the older build re-runs every migration it has.
3. Move any `sessions.corrupt.jsonl` or `groups.corrupt.jsonl` aside rather than deleting it, so step 4 can tell a new one from an old one.
4. With that same older build, run `aoe ps`. It reads every profile directory under `profiles/` and saves none of them, and the only file it adds next to a `sessions.json` is a quarantine file: look for `sessions.corrupt.jsonl` or `groups.corrupt.jsonl` beside each one. One appears as soon as that build reads a row it cannot decode. A profile whose directory is a symlink is skipped by a bare `aoe ps`, so run `aoe ps -p <name>`, or set `AGENT_OF_EMPIRES_PROFILE`, once more for each of those. A missing quarantine file only clears the rows that build could read: a `sessions.json` it cannot parse at all leaves nothing behind either, so check that `aoe ps` lists the sessions you expect. The top-level `sessions.json` from step 1 gets no probe at all, since no release reads it as a session registry. The TUI is not a probe: it reconciles duplicates and rewrites the registry on start.
5. If either file appeared, or `aoe ps` did not list a session you know is there, put your copies back and stay on the current release.

### When an older build could not read your sessions

The quarantine file is a copy of those rows, and it holds one load's worth of them: that load writes the whole batch it could not decode, and a later load replaces the file, so whatever that later load no longer saw is gone from it.

A migration that retypes a persisted field also leaves a `sessions.json.pre-migration-<stamp>` beside the file, holding the bytes from just before that rewrite. When one upgrade retypes more than one field each rewrite leaves its own, and only the first one still holds a shape the older release reads, so restore that one. Do not rank the copies by stamp alone: a stamp is a clock reading, except that a build finding no room above the newest one writes its own below it, so a far-future stamp tells you nothing about age. Three of these are kept per file and the oldest goes first, so a third retype landing in one upgrade evicts the copy the older release reads, and anything else carrying one of those names spends a slot and does the same. They are the only copies a downgrade is served from: repairing an interrupted session move keeps its own `sessions.json.pre-recovery-<stamp>` set, and a repair never evicts a migration backup.

Check that the backup is there rather than assuming it: an upgrade that could not take one says so in its startup output and in the log. These retypes shipped in 1.17.0, so an install that already ran them has no backup of the older shape, and one only helps an install still on 1.16.x that upgrades into a build carrying them. Using it means editing `.schema_version` as in step 2.

## Uninstalling

```bash
aoe uninstall
```

It prompts before removing the binary, the app data directory, and the tmux settings.
