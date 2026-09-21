# Plugins

AoE keeps its core small (sessions, tmux, worktrees) and grows optional capabilities as plugins, so they can be enabled or disabled at runtime. A few first-party plugins ship bundled with the binary; anything else is installed from GitHub or a local directory. Plugins contribute settings and UI, and their workers run through the capability-gated plugin host.

To build one, see [Writing Plugins](development/writing-plugins.md) and the [Plugin API Reference](plugin-api.md).

## Managing plugins

Three equivalent surfaces:

- **CLI**: `aoe plugin list | info <id> | enable <id> | disable <id> | install <source> | update <id> | uninstall <id>`.
- **TUI**: "Manage plugins" in the command palette, or the Plugins settings tab. Space toggles enable and disable.
- **Web**: the Plugins settings tab. Enabling or disabling needs an elevated (passphrase) session when login is enabled and is blocked in read-only mode; localhost browsers skip the passphrase, matching the CLI's same-host trust.

A plugin's enable-state lives under `[plugins."<id>"]` in `config.toml`.

## Bundled plugins

`aoe.web` is the only bundled plugin today: the web dashboard's management marker, present whenever the dashboard is compiled in, so every released binary ships it enabled. Disabling it hides `aoe serve` as an unrecognized subcommand (`--stop`, `--status`, and `--restart` still reach a running daemon); re-enable with `aoe plugin enable aoe.web`. A build without the dashboard has an empty registry, so `aoe plugin list` reporting nothing there is expected.

## Installing external plugins

External plugins are community code you install at your own risk.

```sh
aoe plugin install gh:owner/repo          # latest release (the audited default)
aoe plugin install gh:owner/repo@v1.2.3   # an explicit tag, branch, or commit
aoe plugin install ./path/to/plugin       # a local directory
```

With no `@ref`, install resolves the repo's latest stable GitHub release and records the source ref-less, so `aoe plugin update` keeps tracking releases. An explicit `@ref` installs unverified code, asks you to confirm (`--yes` skips the prompt), and keeps following that ref. A repo with no release falls back to the default branch behind the same confirmation.

A plugin lands in `<app_dir>/plugins/<id>/`, with a GitHub source pinned to its exact commit and any release-binary worker downloaded for your platform. Set `AOE_GITHUB_CLONE_BASE` to install from a GitHub Enterprise host. Resolved versions (commit, manifest hash, release asset) live in `<app_dir>/plugins.lock`, so an install is reproducible.

Every update, including the opt-in auto-update, restarts the plugin's worker in a running `aoe serve`. If the CLI or TUI cannot reach the daemon it warns, and the worker keeps the previous build until the daemon restarts.

The web dashboard's Plugins settings does the same things, with a marketplace searching the `aoe-plugin` GitHub topic; every mutating action confirms the plugin's capabilities first.

## Trust and capabilities

Bundled plugins are `builtin` and fully trusted. Installed plugins are `community` and untrusted: the manifest declares the capabilities they need (network, filesystem, spawning processes, and so on) and install prompts once to grant that exact set. `--yes` grants without prompting, and a capability this version does not recognize is rejected rather than granted. An external plugin cannot claim the reserved `aoe.*` or `agent-of-empires.*` id namespace.

A grant is pinned to the installed manifest, so an update that widens what the plugin can do (new capabilities, changed build steps or UI slots, a runtime or trust change) must be approved before it becomes active. Approve with `aoe plugin update <id>`, or in-app: the web and TUI plugin managers show an Update action with a popup describing what changed. Declining keeps the current version and stops the prompt until the next version. Approval is pinned to the exact fetched content, so an update that changed since you reviewed it is refused rather than applied.

`aoe plugin list` and `aoe plugin info <id>` show each plugin's trust level (`featured`, `community`, or `local`) and whether it is granted.
