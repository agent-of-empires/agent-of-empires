# Development

Install Rust and tmux. Node.js and npm are required only for the web dashboard.

## Build and test

```sh
cargo build                         # debug build: TUI + daemon, no Node
cargo build --release               # shipping build with LTO
cargo build --profile dev-release   # optimized build without LTO
cargo build --features web          # include the web dashboard
cargo test
cargo fmt
cargo clippy
```

The binary is `target/{profile}/aoe`. Web commands and test selection are in
`web/AGENTS.md`.

Debug builds carry line tables only, and dependencies carry no debug info, to
keep `target/` small across worktrees. Panics and backtraces still name the file
and line in this crate; a debugger has no local-variable values. Rebuild with
`RUSTFLAGS="-Cdebuginfo=2"` for a full debugging session.

## Run and inspect logs

```sh
cargo run
AGENT_OF_EMPIRES_DEBUG=1 cargo run
AOE_LOG_LEVEL=trace cargo run
AOE_ACP_TRACE=1 cargo run
AOE_TERMINAL_TRACE=1 cargo run
aoe logs
```

For the dashboard, `cargo xtask dev` runs a dashboard-enabled debug backend on 8081
and Vite with HMR on 5173. Add `--watch` to rebuild and restart the backend when
Rust inputs change. A failed rebuild leaves the previous backend running.

## Build cache across worktrees

Each worktree has its own `target/`. Developers with many worktrees can opt into
[kache](https://github.com/kunobi-ninja/kache), which shares dependency
artifacts through a local content-addressed store. It is optional and is not
used by the repository, CI, Nix, or release builds.

```sh
cargo binstall kache
export RUSTC_WRAPPER=kache
export CARGO_INCREMENTAL=0
cargo build --all-features
```

Use `kache monitor` or `kache stats` to inspect it, and unset `RUSTC_WRAPPER` to
disable it. The cache and worktrees must be on the same filesystem for linking
or reflinking; crates with native linking may still rebuild. Install kache
before exporting `RUSTC_WRAPPER` to avoid a bootstrap loop. To verify shared
artifacts, use `scripts/verify-shared-target.sh`.

## Debug namespace

Debug builds are isolated from installed release state:

| Resource | Release and `dev-release` | Debug |
| --- | --- | --- |
| App dir, macOS and Windows | `~/.agent-of-empires` | `~/.agent-of-empires-dev` |
| App dir, Linux | `~/.config/agent-of-empires` | `~/.config/agent-of-empires-dev` |
| tmux prefix | `aoe_` | `aoe_dev_` |
| serve port | `8080` | `8081` |

Debug tmux also uses an app-directory socket. Override it with
`AOE_TMUX_SOCKET` when needed.

## Demo recordings

The live recorders document their dependencies and setup in their file headers:

- `web/scripts/record-tui-demo.mjs` produces `docs/assets/demo.gif`.
- `web/scripts/record-web-demo.mjs` produces the desktop and mobile dashboard
  GIFs.
