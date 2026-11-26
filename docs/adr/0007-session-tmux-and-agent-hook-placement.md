# Session, Pane lifecycle and Agent hook code go to `aoe-daemon-backend`

Status: proposed. Decides the placement ADR 0001 left open, for #4504 (part of #2734). Terms follow `CONTEXT.md`: **Daemon**, **Client**, **Pane lifecycle**, **Pane I/O**, **Client-only build**.

ADR 0001 splits the root crate into `aoe-daemon-client`, `aoe-daemon-backend`, `aoe-tui` and a thin `aoe` bin, and #4504 requires that `aoe-tui` not depend on `aoe-daemon-backend`. It left open where `src/session`, `src/tmux` and `src/hooks` go. They depend on each other and the TUI uses all three today, so this choice decides whether the compiler holds the TUI to being a Client. We put them in `aoe-daemon-backend` and give the TUI only what crosses the Daemon API, plus Pane I/O.

## Decision

- **Session goes to `aoe-daemon-backend` whole:** runtime, `Instance`, `Storage`, and config loading, saving and layering. Types that cross the Daemon API go to `aoe-daemon-client` as wire types, as #6014 starts to do, including the `Config` structs and settings schema. A Client sees a Session only through wire types such as `SessionResponse`, and reads and writes config through the Daemon, as the Web dashboard already does for `tui_only` settings.
- **tmux splits on the line #3840 drew.** Pane lifecycle (create, kill, respawn, environment sync, tool retirement, status bar) goes to `aoe-daemon-backend`. Pane I/O (attach, capture, send-keys, paste, the VT pipe, OSC 8, the tmux socket locator) goes to a shared crate, `aoe-tmux-io`, used by the TUI and by the Daemon for the Web dashboard terminal.
- **`src/hooks` goes to `aoe-daemon-backend` whole.** Status detection is Daemon work, so the TUI reads hook-derived Status from the Daemon.
- **CLI code that runs an operation locally instead of through the Daemon API (Session, Storage, config or Pane lifecycle) compiles only under the bin's `daemon` feature.** The gate is per subcommand, not per module: `session` mixes local commands with API-backed `favorite` and `empty-trash`. A subcommand that only calls the Daemon API stays ungated, even if it imports Session types, as `send` does. This covers ADR 0004's local-only commands and every command not yet behind an endpoint. Commands that stay local keep the gate; for the rest, each gate goes away when #2734 moves the command behind an endpoint.
- **Other root modules** go to `aoe-daemon-backend` unless `aoe-tui` needs them. Then what crosses the API goes to `aoe-daemon-client`, and local code both sides need goes to a shared crate named for what it holds (ADR 0001).
- **Layering:** the `aoe` bin depends on `aoe-daemon-backend` and `aoe-tui`, they depend on shared crates, and everything may depend on `aoe-daemon-client`. No crate depends on one above it.

## Considered options

- **One shared engine crate** below both `aoe-daemon-backend` and `aoe-tui`, holding session, tmux, hooks and what they import (`acp`, `plugin`, `process`, `migrations`; about 300k lines). Rejected: it meets #4504's dependency rule in name only, because the TUI would still link `Storage`, `Instance` and Pane lifecycle, which ADR 0004 and #3840 move into the Daemon. It rebuilds less after a server-only edit; a later split inside the backend recovers that.
- **Config types and file I/O in a shared crate the TUI reads directly.** Rejected: the Daemon and the TUI would both read the config file, and a TUI on a Remote Daemon would read its own host's file instead of the Daemon's.
- **The TUI does Pane I/O through the Daemon's terminal WebSocket.** Rejected: #3840 keeps attach and pane I/O local to the TUI.
- **Pane I/O in `aoe-daemon-client` behind `cfg(unix)`.** Rejected: it puts local tmux code in the portable wire crate.

## Consequences

- Before `aoe-tui` is extracted, the TUI stops calling session runtime directly, and the back-edges from `session`, `tmux` and `server` into `cli` and `tui` are cut.
- `aoe-daemon-backend` keeps most of today's crate (session, server, `acp`, `plugin`, `migrations`), so a server-only edit still rebuilds all of it. #5522 measured the root crate at 589k lines and found raising cargo jobs from 4 to 8 "barely moved build times". Whether to split the backend further along session, server and `acp` is decided after #4504 from `cargo build --timings`, per its per-leg acceptance.
