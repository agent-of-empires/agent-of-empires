# Development

Install Rust and tmux. Node.js and npm are needed only for the web dashboard.

```sh
cargo build                         # debug: TUI + daemon, no Node
cargo build --release               # shipping build with LTO
cargo build --profile dev-release   # optimized, without LTO
cargo build --features web          # include the web dashboard
cargo test
cargo fmt
cargo clippy
```

Local dashboard end-to-end tests on Linux require `bubblewrap`, `iproute2`, and
`tmux`. Host security policy must allow `CAP_NET_ADMIN` in bubblewrap's children
to configure their private network namespace. The
[web-e2e CI leg](../.github/workflows/tests.yml) requires this coverage with
`AOE_REQUIRE_LOCAL_NETWORK_TESTS=1`; unavailable confinement fails the job.

The core-rest CI leg requires the scoped cancellation case to run with
`AOE_REQUIRE_SCOPED_TESTS=1`; any setup path that skips it must fail. Local runs
may skip the scoped case when no usable user manager exists; unscoped
cancellation still runs.

Creation-progress events are advisory and publish independently of session
snapshots. Tests that need a published session must wait for its ID in the
canonical snapshot. Hold in-flight fixture gates until explicit release, with
cleanup that waits for release acknowledgment and lets a late hook exit when
its gate directory disappears. Keep request deadlines longer than the bounded
scenario and assert the expected API error code, not merely any error. Keep an
emergency hook watchdog beyond the request budget for abrupt test-process exit.

The binary is `target/{profile}/aoe`. Web commands and test selection live in `web/AGENTS.md`.

When adding a mutating REST route, classify it in the CityHall allow or deny table in `src/server/access.rs`. Audit its handler or declare an exact, justified exemption in `src/server/api/mod.rs`.

Debug builds carry line tables only, and dependencies carry no debug info, to keep `target/` small across worktrees. Panics still name the file and line in this crate, but a debugger has no local values; rebuild with `RUSTFLAGS="-Cdebuginfo=2"` for a full debugging session.

## Running and logs

```sh
cargo run
AGENT_OF_EMPIRES_DEBUG=1 cargo run
AOE_LOG_LEVEL=trace cargo run
AOE_ACP_TRACE=1 cargo run
AOE_TERMINAL_TRACE=1 cargo run
aoe logs
```

`cargo xtask dev` runs a dashboard-enabled debug backend on 8081 and Vite with HMR on 5173; `--watch` rebuilds and restarts the backend when Rust inputs change, leaving the previous backend running if a rebuild fails.

Use explicit dotted tracing area targets so per-target logging overrides affect
native and dashboard failures consistently. Settings > Logging exposes server,
migration and process areas; clearing one override restores inheritance without
clearing other targets.

Filter changes saved in Settings > Logging apply to the TUI, running daemon and structured runners in the same app namespace. Sink and rotation changes require restart. No-op and unrelated saves preserve temporary runtime filters.

Debug builds are isolated from installed release state:

| Resource | Release and `dev-release` | Debug |
| --- | --- | --- |
| App dir, macOS | `~/.agent-of-empires` | `~/.agent-of-empires-dev` |
| App dir, Linux | `~/.config/agent-of-empires` | `~/.config/agent-of-empires-dev` |
| tmux prefix | `aoe_` | `aoe_dev_` |
| serve port | `8080` | `8081` |

Debug also uses an app-directory tmux socket, which `AOE_TMUX_SOCKET` overrides.

## Build cache across worktrees

Each worktree has its own `target/`. Developers with many worktrees can opt into [kache](https://github.com/kunobi-ninja/kache), which shares dependency artifacts through a local content-addressed store. It is optional and unused by the repository, CI, Nix, and release builds.

```sh
cargo binstall kache
export RUSTC_WRAPPER=kache
export CARGO_INCREMENTAL=0
cargo build --all-features
```

Install kache before exporting `RUSTC_WRAPPER`, to avoid a bootstrap loop, and unset the variable to disable it. The cache and the worktrees must share a filesystem for linking; crates with native linking may still rebuild. `kache monitor` and `kache stats` inspect it, and `scripts/verify-shared-target.sh` verifies shared artifacts.

## Demo recordings

`web/scripts/record-tui-demo.mjs` produces `docs/assets/demo.gif`, and `web/scripts/record-web-demo.mjs` the desktop and mobile dashboard GIFs. Each documents its dependencies and setup in its file header.
