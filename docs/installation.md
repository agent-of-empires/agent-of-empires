# Installation

**Prerequisites:** [tmux](https://github.com/tmux/tmux/wiki). Docker (or another container runtime) is optional, for [sandboxing](guides/sandbox.md), and [Node.js](https://nodejs.org/) is needed only to build the web dashboard from source. Building also needs a C toolchain for native dependencies; a stock `cc` covers most platforms, and targets without pre-generated AWS-LC bindings also need CMake.

## Install

```bash
# Quick install (Linux and macOS)
curl -fsSL \
  https://raw.githubusercontent.com/agent-of-empires/agent-of-empires/main/scripts/install.sh \
  | bash

# Homebrew
brew install aoe
```

## Build from Source

```bash
git clone https://github.com/agent-of-empires/agent-of-empires
cd agent-of-empires && cargo build --release
```

Add `--features web` for the dashboard, which requires Node and npm. The default-enabled `vendored` feature selects OpenSSL and SQLite vendoring.

For system libraries, install `pkg-config` and compatible libgit2, OpenSSL and SQLite development packages. SQLite must provide JSON functions. Then build with vendoring disabled and system libgit2 required:

```bash
LIBGIT2_NO_VENDOR=1 cargo build --release --no-default-features --features default-plugins
```

`--no-default-features` disables all defaults; explicitly selecting `default-plugins` retains that default feature selection independently of `vendored`. Add `web` to the feature list for the dashboard. `LIBGIT2_NO_VENDOR=1` fails the build if compatible system libgit2 is unavailable, instead of falling back to a source build. Other native dependencies are unaffected, and system libgit2 may still link OpenSSL transitively. The passkey sign-in dependency also links OpenSSL, which then comes from the system.

A source build leaves the binary at `target/release/aoe`. Verify any install with `aoe --version`.

## Updating

```bash
aoe update
```

`aoe update` detects how aoe was installed (Homebrew, the install script, Nix, or Cargo) and dispatches to the right mechanism. For Nix and Cargo it prints the manual command instead, since those need external tooling. A Nix profile install updates with `nix profile upgrade agent-of-empires` (see `nix profile list` for the entry name); if aoe comes from a flake input, run `nix flake update <input>` (your flake's name for aoe) and rebuild your system or home-manager configuration. In the TUI, press `u` while the update bar is visible to run the same flow, or `Ctrl+x` to dismiss the bar.

A schema upgrade refuses to run while a daemon is still writing the old data layout. The TUI and ordinary commands stop before migration and tell you how to continue. With the new binary installed, run `aoe serve --restart` to retain the managed daemon's launch settings and authentication, stop its original process, migrate under exclusion, and start its replacement. Alternatively, run `aoe serve --stop`, then reopen the TUI. Stop a service-supervised daemon through its service manager before migrating and restart it there afterwards. Do not remove PID files or schema markers to bypass this gate.

A daemon from an older build may not expose the local Unix runtime transport, even when a previous attempt already advanced the schema marker. The TUI asks you to restart that daemon rather than trying to use an incompatible transport. Stop and restart capture the original process incarnation before signaling; an exited but unreaped daemon does not require unsafe PID-based escalation.

If you installed shell completions as a static file, regenerate it afterwards so it picks up new commands and flags; see [Shell completions](guides/shell-completions.md) for the always-fresh setup that avoids this.

## Downgrading

Downgrades are unsupported. An older build refuses a newer `.schema_version`; preserve that refusal. Never lower or delete the marker to force startup. Older commands and clients are not safe readability probes against current application data.

Before upgrading, stop the daemon normally and quiesce clients and other publishers. Keep a consistent backup of the complete [application directory](guides/configuration.md#file-locations), including the schema marker, every profile, ownership journals, worker and launch records, credentials and conversation bindings. Include external registry targets, native conversation stores and other referenced data that the application directory does not contain. Restoring only session rows can separate resources from their original authority.

Per-file `sessions.json.pre-migration-<stamp>` backups preserve bytes before individual rewrites, not a complete installation snapshot. Retention is bounded and later rewrites can evict the old shape. Quarantine files preserve damaged evidence, not a downgrade backup or proof of readability. Do not combine older rows with newer journals or manufacture missing receipts. Keep the current release unless a consistent pre-upgrade installation and its referenced data can be restored; such restoration is not a supported in-place downgrade.

## Ownership recovery

Stop the daemon with `aoe serve --stop` and quiesce every publisher before recovery. Resolve ambiguous daemon identity first. Do not delete PID records, locks, rows, tokens or credentials to force admission. Future schemas remain refused. Recovery uses serve, lifetime and state exclusion; a torn inactive historical launch record can be quarantined without removing a healthy peer record or its credentials.

### Restore an ownership journal

A missing or corrupt `pending-purge-owners.json` or `tool-retirements.json` fences cleanup. Restore only a complete original journal from a consistent backup of this application namespace. It must contain every outstanding original owner, frozen request, protection path, process birth and scope receipt. Valid JSON alone does not prove completeness.

```sh
aoe migrate --restore-purge-journal /backup/pending-purge-owners.json --confirm-complete-original-authority
# Alternatively, restore original tool retirement receipts:
aoe migrate --restore-tool-journal /backup/tool-retirements.json --confirm-complete-original-authority
```

Run only the matching restore command. Restore modes are mutually exclusive and require explicit confirmation. They restore authority only: no startup, process stop, cleanup, row removal or schema-marker update. Healthy or unsupported destinations are refused. Corrupt destination bytes remain in private quarantine files. A durability error can occur after publication; inspect the result rather than overwriting a now-healthy journal. Without a complete original backup, retain the fence. Never initialize an empty journal or manufacture missing proof.

### Resolve legacy unknown scopes

Stop, remove, purge and start resolve a pre-upgrade terminal session's scopes themselves. Units in the naming format earlier releases used, `aoe-session-<id>-<start_ms>.scope`, are adopted and stopped as those releases did. Any other live unknown unit is retained, not adopted or stopped. For worker and journal-held debt, repair unreadable components, restore required journals and run `aoe migrate`; a current schema marker is not proof.

On Linux with a local systemd user manager, scope recovery authenticates that manager through its kernel-identified private socket. It binds the manager's reported cgroup root to its reader-relative procfs path and verifies the same physical manager in that directory's kernel membership. Recovery scans the complete manager subtree on a root-mounted cgroup2 filesystem without descendant overmounts. It does not require access to root's or the manager's protected namespace links. Unreadable or partial evidence, a changing manager, and live unknown units still refuse recovery. The runtime namespace comes from the effective user, not a caller-selected recovery identity.

Managerless Linux operation resolves legacy rows to no scopes when `/run/user` or `/run/user/<euid>` is positively absent beneath validated canonical ancestors. If the canonical runtime exists but its `systemd/` directory is absent, recovery additionally requires a complete physical cgroup2 hierarchy with no populated scope matching that session. This supports elogind-style hosts without trusting missing user-owned sockets alone.

The kernel-only proof checks the cgroup2 filesystem, complete mount boundary, physical-root interfaces and namespace stability, then scans the full hierarchy without exempting captured units. A populated matching scope, unreadable or partial inventory, symlink, or changing boundary retains unresolved authority and resources. Existing `systemd/` with an unavailable private socket still refuses recovery. Kernel discovery never grants retirement authority or changes original captured receipts. Caller-selected `XDG_RUNTIME_DIR`, disabling scope creation and a failed capability probe are not absence proof.

Unsupported or incomplete evidence remains protected: cgroup v1, hybrid layouts without the required root-mounted cgroup2 view, and views exposing only a delegated subtree do not establish absence. If the caller's namespace restricts that view, retry from the original host and context exposing the complete original hierarchy. Do not reboot, switch cgroup modes or manufacture receipts to make unknown authority disappear. Without supported original evidence, retain the resources; automatic v1 recovery and operator-attestation overrides are not provided.

On macOS, recovery verifies the native Darwin kernel, which cannot host Linux systemd cgroups. This resolves only missing Linux-scope evidence. Explicit captured Linux unit receipts remain refused on macOS; recover those original resources on their Linux host. Worker PID, birth, generation, native runtime, producer and tmux authority are not adopted or replaced. Their own stop and cleanup guards still apply.

Explicit recovery preserves original worker records and opaque fields, changing only an unresolved scope to proven unscoped status. A missing ownership journal must be restored from complete original authority, never recreated empty.

If a live unknown unit blocks recovery:

1. Keep a consistent original backup and keep all candidate-unit publishers quiescent. Discover units without changing them:
   ```sh
   systemctl --user list-units --all --plain --no-legend --full --type=scope
   ```
2. Select one literal candidate name as `UNIT` and inspect its identity:
   ```sh
   systemctl --user show --property=Id --property=InvocationID --property=ActiveState --property=ControlGroup -- "$UNIT"
   ```
   Inspect surviving original launch evidence and the complete cgroup and process-birth inventory. A matching session ID, pane PID, absent pane or current configuration alone does not prove ownership.
3. Independently establish that this exact current unit incarnation is the original intended cohort. If original ownership or incarnation cannot be proved, or another application may own or recreate it, stop here. Do not retire it or clear unknown debt.
4. Only after establishing that authority, retire that literal original unit:
   ```sh
   systemctl --user stop -- "$UNIT"
   systemctl --user show --property=ActiveState --property=ControlGroup -- "$UNIT"
   ```
   Require inactive or failed state and an empty cgroup. Command success alone is not proof. Never use a glob or a destructive list derived only from session-ID matching.
5. Run `aoe migrate` again for fresh complete manager and kernel proof. Only proven absence resolves unknown scope debt. Captured owned units and all process-birth, native-runtime, producer and filesystem authority remain unchanged.

If complete absence or original ownership cannot be established, recovery remains fenced. Do not remove evidence to make it pass.

## Uninstalling

```bash
aoe uninstall
```

It prompts before removing the binary, the app data directory, and the tmux settings.
