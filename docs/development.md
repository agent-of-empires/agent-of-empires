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

The binary is `target/{profile}/aoe`. Web commands and test selection live in `web/AGENTS.md`.

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
