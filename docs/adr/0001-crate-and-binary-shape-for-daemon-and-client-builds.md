# One `aoe` binary, split into Daemon and Client workspace crates

Status: proposed. Decides #4398 (part of #2734). Terms follow `CONTEXT.md`: **Daemon** is the `aoe serve` process, a **Client** is the TUI, CLI or Web dashboard that pilots it.

The Daemon and the Clients must build separately so a Client-only build, first the CLI on Windows, never compiles `nix`, tmux or axum. We keep one `aoe` binary and one command name, and draw the boundary with workspace crates, not with runtime dispatch or `cfg` on a single crate.

## Decision

- **One binary, in-process modules.** `aoe` stays a single executable. `serve`, the TUI (bare `aoe`) and the CLI (`aoe <options>`) are in-process modes of it, not external `aoe-<name>` executables found on `PATH`.
- **Layout `daemon/client` and `daemon/backend`.** Following the thread on #4395, the crates are split by which side of the Daemon API they sit on:
  - `aoe-daemon-client`: the wire types and the HTTP client for the Daemon API. Every Client depends on it. It carries no `session`, `tmux`, `nix` or `axum` dependency and compiles on Windows.
  - `aoe-daemon-backend`: the Daemon itself (`aoe serve`, API, Supervisor, Workers, tmux ownership). Unix only.
  - `aoe-tui`: the TUI Client. Unix only for now.
  - Shared crates are added only when two of the above need the same code, and are named for what they hold.
- **Client-only selection is a Cargo feature on the thin `aoe` bin package.** Default features `daemon` and `tui` pull in `aoe-daemon-backend` and `aoe-tui`. A Client-only build, such as the Windows CLI, is `--no-default-features`. The library crates carry no such features.
- **Bare `aoe` is the TUI**, unchanged. `aoe <options>` is the CLI that manages the Daemon. With the TUI compiled out, bare `aoe` prints help and exits non-zero.
- **First extraction is `aoe-daemon-client`**, from `src/daemon/`. Move wire types and the portable HTTP client first. Isolate the current `session`, `tmux`, `axum` and Unix-only edges before claiming Client-only Windows support.
- **CI gate:** `cargo check --target x86_64-pc-windows-msvc` on `aoe-daemon-client` from day one, so POSIX code cannot leak back into the Client graph. The same gate on the Client-only `aoe` lands once the CLI's `session` and `tmux` edges are cut (`src/cli` imports `session` at about 296 sites and `tmux` at 75).

## Considered options

- **Cargo features on the single `aoe` crate.** Rejected: `src/lib.rs` declares 29 modules unconditionally and `session` is the densest edge (`session -> tmux` 314 use sites, `tmux -> session` 81 back), so a feature flag gates nothing the compiler can enforce. Each feature set also costs a full extra `target/` copy.
- **Git-style dispatch to external `aoe-<name>` executables.** Rejected: a large refactor for little UX gain. In-process modularity with Rust crates gives the same Client-only install without cross-executable duplication or a new process boundary.
- **Separate bin names (`aoe`, `aoe-tui`, `aoed`).** Rejected: it breaks the single command name and the rule that `aoe` with no config on one machine keeps working. Auto-start also needs one binary: `aoe serve --daemon` re-execs the current executable.

## Consequences

- The existing `src/daemon/` module is the client side of the Daemon API, so the name `daemon` in code already means "Daemon client". Renaming it is a follow-up, as already scoped in #2734.
- Wire DTOs own serialization, not backend conversion. `src/server/api/sessions/model.rs` converts a backend Session (`Instance`) and its runtime inputs into `SessionResponse`; the wire type has no inherent backend constructors.
- The current monolith reuses `reqwest` for TCP. `hyper` drives HTTP/1 on the exact UID/PID-checked Unix stream, with `hyper-util`'s `TokioIo` adapter. Plain and `web` builds both include this transport; `web` adds dashboard assets. The future Client crate must own the portable HTTP dependencies and target-gate its Unix adapter without importing backend or `session` edges. This PR does not implement that extraction or verify Windows builds.
- Where `session`, `tmux` and `hooks` sit relative to `aoe-daemon-client` is not decided here (see the Not yet specified section of #2734).
- Explicit `aoe tui` and `aoe web` subcommands are a follow-up.
- `main.rs` runs migrations for nearly every subcommand. A Client-only build has no local store to migrate, so it skips them and does not depend on `migrations`. Cutting that dependency is part of the CLI `session` edge work.
- Windows ships the Client-only `aoe`. It needs a C toolchain for the `-sys` crates (`libgit2-sys`, `libsqlite3-sys`, `aws-lc-sys` and others) until those are made optional.

Evidence: `docs/research/2734-windows-client-build.md` on `research/4397-windows-client-build`.
