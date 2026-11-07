# Plugins

AoE keeps its core small (sessions, tmux, worktrees) and grows optional capabilities as plugins, so they can be enabled or disabled at runtime. A few first-party plugins ship bundled with the binary; anything else is installed from GitHub or a local directory. Plugins contribute settings and UI, and their workers run through the capability-gated plugin host.

To build one, see [Writing Plugins](development/writing-plugins.md) and the [Plugin API Reference](plugin-api.md).

## Managing plugins

Three equivalent surfaces:

- **CLI**: `aoe plugin list | info <id> | logs <id> | enable <id> | disable <id> | install <source> | update <id> | uninstall <id>`.
- **TUI**: "Manage plugins" in the command palette, or the Plugins settings tab. Space toggles enable and disable.
- **Web**: the Plugins settings tab. Enabling or disabling needs an elevated (passphrase) session when login is enabled and is blocked in read-only mode; localhost browsers skip the passphrase, matching the CLI's same-host trust.

A plugin's enable-state lives under `[plugins."<id>"]` in `config.toml`.

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

CLI install, update, and uninstall run only through the Daemon. With no explicit
endpoint, the CLI starts the local Daemon when needed. `--no-autostart` refuses
instead; `--daemon-url` and `AOE_DAEMON_URL` never start a local replacement.
Read-only, CityHall, authentication, and elevation restrictions still apply.

`plugin list`, `info`, `logs`, and `outdated` inspect the local installation when
no endpoint is selected. They refuse an explicit endpoint, including loopback TCP,
rather than displaying unrelated Client state. Use that Daemon's `/api/plugins`
API or run the inspection on its host without the endpoint override.

Local directories, including `--dev`, require the owner-authenticated local Unix
socket. The CLI resolves relative paths from its working directory; the Daemon
reads and copies the source. Explicit remote endpoints reject Client paths,
including loopback TCP URLs. GitHub sources use `gh:owner/repo[@ref]`, not arbitrary
Git URLs. Updates retain the recorded source and development marker.

Preview may fetch into temporary staging but never builds, provisions Node,
grants capabilities, or publishes the plugin. Approval pins the fetched content;
`--yes` skips prompts, not the fingerprint check.
Source-tree preparation and cleanup run off Daemon runtime workers. Cancellation
keeps mutation exclusion until an already-running preparation finishes.
Explicit CLI updates reapply even unchanged sources, so they can restore missing
plugin files. They use the preview fingerprint and preserve the enabled state.

An incompatible or malformed update preview stops before submission. Check the
Client and Daemon versions, then upgrade and restart the Daemon on its host with
`aoe serve --restart`, or restart its foreground process or service manager.

Each accepted operation prints a job id and waits for its result. Resume observation
with `aoe plugin job <job_id>` using the same Daemon endpoint and credentials.
A polling failure, interrupted observation, or lost submission response means the
outcome is unknown, not permission to retry. The CLI never resubmits automatically.
If no job id arrived, inspect `GET /api/plugins` on that Daemon before deciding
whether to submit again. Jobs expire after an hour when another job starts and
do not survive a Daemon restart; a missing job requires inspecting current state.

Successful install and uninstall jobs reconcile the live worker set. Installation
schedules worker startup; job success does not mean the worker is ready.

Lifecycle and approval changes share exclusion across the Daemon, local TUI
operations and startup sweeps. Contention is refused, never queued. A competing
process can make an accepted job fail before preparation; wait for that operation
and inspect current state before submitting another request.

A plugin lands in `<app_dir>/plugins/<id>/`, with a GitHub source pinned to its exact commit and any release-binary worker downloaded for your platform. Set `AOE_GITHUB_CLONE_BASE` to install from a GitHub Enterprise host. Resolved versions (commit, manifest hash, release asset) live in `<app_dir>/plugins.lock`, so an install is reproducible.

Build steps and workers run with `AOE_NODE` set to the resolved Node and its directory first on `PATH`. If no Node is found when a plugin declares a build step or a non-system worker, install downloads the pinned one on Linux and macOS. Elsewhere, put Node on `PATH` or set `AOE_ACP_NODE`.

Every update, including the opt-in auto-update, restarts the plugin's worker in a running `aoe serve`. CLI updates apply and reload the worker within the Daemon job. The TUI's local update path warns if it cannot reach the Daemon; the worker then keeps the previous build until the Daemon restarts.

The web dashboard's Plugins settings does the same things, with a marketplace searching the `aoe-plugin` GitHub topic; every mutating action confirms the plugin's capabilities first.

## Trust and capabilities

Bundled plugins are `builtin` and fully trusted. Installed plugins are `community` and untrusted: the manifest declares the capabilities they need (network, filesystem, spawning processes, and so on) and install prompts once to grant that exact set. `--yes` grants without prompting, and a capability this version does not recognize is rejected rather than granted. An external plugin cannot claim the reserved `aoe.*` or `agent-of-empires.*` id namespace unless its release hash is pinned in the featured index. `aoe plugin install --dev <dir>` is the exception for a local checkout: the plugin is marked `dev`, is never featured, stays `dev` across `aoe plugin update`, and `aoe serve --remote` refuses to start while one is active.

A grant is pinned to the installed manifest, so an update that widens what the plugin can do (new capabilities, changed build steps or UI slots, a runtime or trust change) must be approved before it becomes active. Approve with `aoe plugin update <id>` (`--yes` skips the prompt), or in-app: the web and TUI plugin managers show an Update action with a popup describing what changed. Declining keeps the current version and stops the prompt until the next version. Approval is pinned to the exact fetched content, so an update that changed since you reviewed it is refused rather than applied.

Update also offers fingerprint-bound repair when the source is unchanged but
the installed manifest or its approval is invalid. Refusing leaves it inactive;
clean-only auto-update never approves that repair. A healthy disabled plugin
remains disabled and does not need repair.

`aoe plugin list` and `aoe plugin info <id>` show each plugin's validation (`builtin`, `featured`, `community`, `local`, or `dev`) and whether it is granted.
