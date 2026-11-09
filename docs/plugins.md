# Plugins

AoE keeps its core small (sessions, tmux, worktrees) and grows optional capabilities as plugins, so they can be enabled or disabled at runtime. A few first-party plugins ship bundled with the binary; anything else is installed from GitHub or a local directory. Plugins contribute settings and UI, and their workers run through the capability-gated plugin host.

To build one, see [Writing Plugins](development/writing-plugins.md) and the [Plugin API Reference](plugin-api.md).

## Managing plugins

Manage plugins through these surfaces:

- **CLI**: `aoe plugin list | info <id> | logs <id> | enable <id> | disable <id> | install <source> | update <id> | uninstall <id>`.
- **TUI**: "Manage plugins" in the command palette applies Space toggles immediately. In the Plugins settings tab, Space stages a change and Ctrl-S saves it.
- **Web**: the Plugins settings tab. Enabling or disabling needs an elevated (passphrase) session when login is enabled and is blocked in read-only mode.

A plugin's enable-state lives under `[plugins."<id>"]` in `config.toml`.

### Web plugin errors

The command-palette manager can save locally when initial local-daemon discovery fails. Once discovery succeeds, any error while changing `aoe.web` is reported without retrying the write locally. This includes lifecycle-busy and read-only refusals, authentication errors, an unavailable transport, and a missing endpoint on an older daemon. Other plugins retain their local-save fallback with a stale-daemon warning.

A repeated Boolean write is idempotent, but a configuration lock does not enforce daemon authorization or reconcile live workers. A failed response is not permission to bypass that daemon. For persistent local transport or HTTP 404 failures, run `aoe serve --stop`, inspect `aoe plugin info aoe.web`, then explicitly retry the desired `aoe plugin enable aoe.web` or `aoe plugin disable aoe.web` command if still needed. Stopping disconnects daemon clients; supervised daemons must be stopped through their manager. Retry may autostart using current configuration. For remote endpoints, recover the selected daemon on its host instead. Lifecycle-busy, read-only and authentication refusals remain authoritative.

`aoe plugin enable` and `aoe plugin disable` also report daemon errors without a local fallback. If the CLI fails to acquire an endpoint for a live owner with a missing socket, it reports the acquisition error without adding the stop/retry hint. The command-palette manager can instead reach the failed request and show the hint.

The TUI Plugins settings tab remains an explicit local configuration save, separate from the command-palette manager's immediate daemon request.

## Worker logs

A plugin worker's stderr goes to `<app_dir>/plugin-workers/<plugin-id>_<worker-uuid>.log`, one file per worker start. `aoe plugin logs <id>` prints the path of the most recent one, which is the live worker's while it runs, so `tail -f "$(aoe plugin logs <id>)"` follows it. Workers only run under `aoe serve`, so a plugin that never ran has no log.

The host keeps the five newest logs per plugin and removes older ones when a worker starts; a running worker's log is never removed. Logs from before names carried the plugin id are removed once a week old.

## Bundled plugins

`aoe.web` is the only bundled plugin today: the web dashboard's management marker, present whenever the dashboard is compiled in, so every released binary ships it enabled. Disabling it hides `aoe serve` as an unrecognized subcommand (`--stop`, `--status`, and `--restart` still reach a running daemon); re-enable with `aoe plugin enable aoe.web`. A build without the dashboard has an empty registry, so `aoe plugin list` reporting nothing there is expected.

## Featured plugins

Featured plugins are first-party plugins whose release hashes are pinned in `plugins/featured.toml`, so `aoe plugin list` shows them as `featured`.

- **Board** (`agent-of-empires.kanban`): a kanban board of your sessions in the web dashboard. Install with `aoe plugin install gh:agent-of-empires/plugin-kanban`; it needs Node 22 or newer on the daemon's `PATH`.

## Installing external plugins

External plugins are community code you install at your own risk.

```sh
aoe plugin install gh:owner/repo          # latest release (the audited default)
aoe plugin install gh:owner/repo@v1.2.3   # an explicit tag, branch, or commit
aoe plugin install ./path/to/plugin       # a local directory
```

With no `@ref`, install resolves the repo's latest stable GitHub release and records the source ref-less, so `aoe plugin update` keeps tracking releases. An explicit `@ref` installs unverified code, asks you to confirm (`--yes` skips the prompt), and keeps following that ref. A repo with no release falls back to the default branch behind the same confirmation.

A plugin lands in `<app_dir>/plugins/<id>/`, with a GitHub source pinned to its exact commit and any release-binary worker downloaded for your platform. Set `AOE_GITHUB_CLONE_BASE` to install from a GitHub Enterprise host. Resolved versions (commit, manifest hash, release asset) live in `<app_dir>/plugins.lock`, so an install is reproducible.

Build steps and workers run with `AOE_NODE` set to the resolved Node and its directory first on `PATH`. If no Node is found when a plugin declares a build step or a non-system worker, install downloads the pinned one on Linux and macOS. Elsewhere, put Node on `PATH` or set `AOE_ACP_NODE`.

Every update, including the opt-in auto-update, restarts the plugin's worker in a running `aoe serve`. If the CLI or TUI cannot reach the daemon it warns, and the worker keeps the previous build until the daemon restarts.

The web dashboard's Plugins settings does the same things, with a marketplace searching the `aoe-plugin` GitHub topic; every mutating action confirms the plugin's capabilities first.

## Trust and capabilities

Bundled plugins are `builtin` and fully trusted. Installed plugins are `community` and untrusted: the manifest declares the capabilities they need (network, filesystem, spawning processes, and so on) and install prompts once to grant that exact set. `--yes` grants without prompting, and a capability this version does not recognize is rejected rather than granted. An external plugin cannot claim the reserved `aoe.*` or `agent-of-empires.*` id namespace unless its release hash is pinned in the featured index. `aoe plugin install --dev <dir>` is the exception for a local checkout: the plugin is marked `dev`, is never featured, stays `dev` across `aoe plugin update`, and `aoe serve --remote` refuses to start while one is active.

A grant is pinned to the installed manifest, so an update that widens what the plugin can do (new capabilities, changed build steps or UI slots, a runtime or trust change) must be approved before it becomes active. Approve with `aoe plugin update <id>` (`--yes` skips the prompt), or in-app: the web and TUI plugin managers show an Update action with a popup describing what changed. Declining keeps the current version and stops the prompt until the next version. Approval is pinned to the exact fetched content, so an update that changed since you reviewed it is refused rather than applied.

`aoe plugin list` and `aoe plugin info <id>` show each plugin's validation (`builtin`, `featured`, `community`, `local`, or `dev`) and whether it is granted.
