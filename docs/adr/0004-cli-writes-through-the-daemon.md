# Route CLI writes through the Daemon and auto-start it

Status: proposed. Decides #4400 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it.

Today almost every mutating CLI command writes `sessions.json`, tmux, worktrees or config files itself, and the running Daemon only learns about it through its `sessions.json` watcher. That out-of-band write is the stale-reload eviction of #4035 and the race class of #2541. #3838 made the Daemon mandatory and auto-started for the TUI; ADR 0003 forbids a disk fallback for mutations and ties removal of the read fallback to CLI auto-start. This ADR sets which CLI commands route through the Daemon and what the CLI does when none runs.

## Decision

- **The CLI auto-starts a local Daemon** when it needs one and none answers, then hard fails with reconnect guidance if the start fails. The started Daemon is the one #3838 describes: detached, shared, Unix socket only, no idle exit; the Web dashboard and Tunnel follow the Web serving and Tunnel settings (off by default). Until the socket listener lands (#3897 on Linux, #4431 on macOS) it binds loopback TCP and is discovered through `serve.url`, as `aoe serve --daemon` does today, so auto-start does not wait on it. The CLI and the TUI share one spawn path; when several Clients race, exactly one Daemon wins and the others connect to it.
- **Auto-start is local only.** With `AOE_DAEMON_URL` or `--daemon-url` set, the CLI never spawns and hard fails when the remote Daemon is unreachable. A Client-only build (Windows) has no local Daemon, so without a URL it hard fails with guidance.
- **Opt-out:** a global `--no-autostart` flag, also read from `AOE_NO_AUTOSTART=1`, honored by the CLI and the TUI. With it, a missing Daemon is a hard fail.
- **Auto-start is announced** with one line on stderr (`Started aoe daemon (pid N)`), also under `--json`, so stdout stays parseable.
- **Session state goes only through the Daemon:** `add`, `remove`, `send`, every `session` subcommand, `group`, `worktree cleanup`, and `acp stop|kill|restart`. Commands without an endpoint get one; `session empty-trash` gets a bulk purge endpoint rather than a Client-side loop, which would reopen #2541. A migrated command has no local path.
- **Config the Daemon reads goes only through the Daemon:** `profile`, `project`, `skill`, `plugin` (its local fallback in `plugin enable|disable` is removed), `telemetry` and `cityhall apply`. The Daemon is the single writer of what it consumes, so no config watcher is needed and remote Clients work.
- **Stays local, never auto-starts:**
  - Client assets: `theme export`, `sounds install`.
  - Files on the Daemon host: `init`, `hooks approve`. They only make sense when the Client runs on that host.
  - Daemon and install management: `killall`, `update`, `uninstall`, `migrate`, `sandbox reclaim`, `serve`. `migrate` refuses while a Daemon runs.
  - `log-level` only changes a running Daemon, so it errors instead of starting one.
- **Migration is per command, `add` first,** with no interim notify for commands not yet migrated. Until `add` is migrated, a request that resolves a just-added session can still 404 inside #4035's window; the Daemon's next reload restores the row but does not retry that request. The window closes once no Client writes out of band.
- **The `aoe serve` flags stay the control surface** (`--daemon`, `--stop`, `--status`, `--restart`). `--stop` stops the Daemon only until the next Client call auto-starts it, unless `--no-autostart` is set; Workers and tmux sessions outlive it.
- **Web serving and Tunnel are Daemon settings,** so an auto-started Daemon honors their persisted values. They are written through the Daemon like other config it reads, and changing one makes the Daemon perform the coordinated restart from #3838. `aoe serve` exposure flags (`--remote`, `--host`, `--port`, `--auth`) against a running Daemon update these settings instead of bailing.

## Considered options

- **Error instead of auto-start.** Rejected: `aoe add` in a fresh shell would fail, breaking "`aoe` with no config keeps working", and ADR 0003's read fallback could never be removed.
- **Spawn a local Daemon when the remote one is unreachable.** Rejected: it would silently write to the wrong store.
- **Config edits stay local, Daemon watches config files.** Rejected: two writers again, and no answer for remote Clients.
- **Client loops over per-session endpoints for bulk operations.** Rejected: the loop runs on a stale snapshot, the #2541 race.
- **Notify the Daemon after out-of-band writes during migration.** Rejected: plumbing thrown away once every command is migrated.

## Consequences

- Once auto-start lands, ADR 0003's three disk read paths are removed, and a CLI that cannot reach the Daemon hard fails.
- `require_daemon` stops refusing to spawn; the TUI's confirm-before-start prompt for structured views is replaced by the shared auto-start.
- `aoe daemon start|stop|status|restart` verbs and launchd/systemd service units are low-priority follow-ups.
- Running `init` and `hooks approve` against a remote Daemon is left to the remote registry work in #2734.
