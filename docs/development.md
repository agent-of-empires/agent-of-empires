# Development

## Building

```bash
cargo build                    # Debug build
cargo build --release          # Release build (with LTO)
cargo build --profile dev-release  # Optimized build without LTO (faster compile)
```

The release binary is at `target/release/aoe`.

The web dashboard needs the `serve` feature and Node.js: `cargo build --release --features serve`. See [Web Dashboard Development](development/web-dashboard.md).

## Faster rebuilds across worktrees (kache)

AoE's normal workflow keeps several git worktrees in flight at once (one per
branch). Each worktree has its own `target/`, so a cold `cargo build` in a fresh
worktree recompiles all ~400 dependency crates from scratch, even when
`Cargo.lock` is identical to a worktree that built them minutes ago. That is
both slow (minutes of dependency compilation) and wasteful on disk (each
`target/` holds its own multi-gigabyte copy of the same artifacts).

[kache](https://github.com/kunobi-ninja/kache) is an optional, opt-in fix for
this. It is a content-addressed rustc wrapper: it compiles each crate once,
stores the artifact in a shared local store (`~/Library/Caches/kache` on macOS,
`~/.cache/kache` on Linux), and **shares it into each worktree's `target/`
without copying** by reflinking it (a copy-on-write clone, on APFS/btrfs/xfs) or
hardlinking it (on filesystems without reflink support, such as ext4). The first
build of a given `Cargo.lock` populates the store; every later build, in any
worktree, restores the dependency artifacts instead of recompiling them. Each
worktree still produces its own `target/debug/aoe` binary, and the shared blocks
mean one physical copy of each dependency artifact backs all worktrees, so both
build time and disk usage drop.

kache is **not** required and is deliberately **not** committed to the repo (no
`rustc-wrapper` in `.cargo/config.toml`): a plain `cargo build` with no extra
setup works exactly as it does today, and CI, the Nix build, and release builds
never touch kache, so shipped release binaries stay compiled by plain `rustc`.
Each developer turns it on for themselves with two environment variables.

### Opting in

Install a prebuilt binary (the prebuilt avoids compiling kache; see the
bootstrap caveat below):

```bash
cargo binstall kache         # prebuilt binary, recommended
# or
mise use -g github:kunobi-ninja/kache@latest
```

Enable it for your shell (for example in `~/.zshrc` or `~/.bashrc`):

```bash
export RUSTC_WRAPPER=kache
export CARGO_INCREMENTAL=0   # kache and incremental compilation are mutually exclusive
```

Then build as usual; `rustc` now routes through kache and your per-worktree
`target/debug/aoe` is unchanged:

```bash
cargo build --all-features
```

Watch cache hits and deduplicated bytes live with `kache monitor`, or print a
non-interactive summary with `kache stats`. To go back to plain `rustc`, unset
`RUSTC_WRAPPER` (or point it at another wrapper, e.g. `export
RUSTC_WRAPPER=sccache` for a time-only cache without disk dedup).

### Caveats

- **Same filesystem only.** Reflinks and hardlinks cannot span filesystems, so
  the kache store and your worktrees' `target/` dirs must live on one volume. A
  worktree on a different mount falls back to copying (still cached, no disk
  dedup).
- **Native-linking crates still rebuild.** Dependencies with build scripts that
  link C libraries (`git2`, `openssl-sys`, `ring`) are not cacheable and
  recompile each time. They are a minority of total build time.
- **`--features serve` and stale assets.** The web dashboard embeds `web/dist`
  at compile time via `rust-embed` (the `debug-embed` feature embeds in debug
  builds too). `build.rs` emits `rerun-if-changed=web/src` (and the other web
  inputs), so rebuilding the frontend dirties the crate and kache recompiles it;
  a cache hit therefore should not serve stale embedded assets. If you ever
  suspect a stale bundle, `cargo clean -p agent-of-empires` forces a rebuild.
- **macOS:** kache excludes its store from Spotlight indexing and Time Machine
  automatically.
- **Bootstrap.** Prefer the prebuilt install above. If you do build kache from
  source with `cargo install` while `RUSTC_WRAPPER=kache` is already exported,
  cargo tries to use kache to compile kache and fails; unset the variable for
  that one command.

You can confirm the dependency artifacts are actually shared and deduplicated
across two target dirs with `scripts/verify-shared-target.sh` (see the script
header for usage).

## Running

```bash
cargo run --release            # Run from source
AGENT_OF_EMPIRES_DEBUG=1 cargo run  # Debug logging (writes to debug.log in app data dir)
AOE_LOG_LEVEL=trace cargo run        # Pick the log level explicitly
AOE_ACP_TRACE=1 cargo run            # Plus raw ACP JSON-RPC firehose; useful for
                                     # verifying sub-agent linkage
                                     # (`_meta.claudeCode.parentToolUseId` round-trip)
                                     # and other adapter-side _meta fields. Structured view
                                     # also logs a `acp.protocol.tool_dispatch` debug line whenever
                                     # it links a child tool call to a parent Task.
AOE_TERMINAL_TRACE=1 cargo run       # Plus per-message bytes for the web terminal WS (spammy)
aoe logs                       # View debug.log via lnav/bat/less (auto-detects)
aoe logs --path                # Print the resolved log file path
```

Requires `tmux` to be installed.

### Web dashboard dev server

```bash
cargo xtask dev    # Unix only
```

Builds the serve-enabled binary, then runs `aoe serve` (8081) and the Vite dev
server (5173) together with hot module reload. Open
[http://localhost:5173](http://localhost:5173); Vite proxies `/api` and the
AoE session WebSocket relays to the backend (via `VITE_PROXY`). One Ctrl-C
stops both. Ports are overridable with `--serve-port` / `--web-port`. See
[Web Dashboard Development](development/web-dashboard.md#manual-frontend-loop)
for the manual two-shell alternative.

Add `--watch` to auto-rebuild the Rust backend on source edits:

```bash
cargo xtask dev --watch
```

It watches `src/**`, `Cargo.toml`, and `Cargo.lock`; on a change it runs
`cargo build --features serve` and, if that succeeds, restarts `aoe serve`. A
failed build leaves the running backend in place and prints the error. The Vite
dev server is never restarted, so frontend HMR keeps working and the browser
reconnects through the proxy once the backend is back. Note that the backend
restart drops all live terminal and structured-view WebSocket connections.

### Dev namespace

Debug builds use an isolated namespace so a local `cargo run` shares no state with an installed release `aoe`; run them side-by-side without colliding on sessions, settings, tmux, or `aoe serve`. `debug.log` lives in the app dir, so it's isolated too. The dev namespace starts empty (nothing migrates from your real dir); wipe it any time with `rm -rf ~/.agent-of-empires-dev` (Linux: the XDG equivalent).

| | Release | Debug (`cargo run`) |
| --- | --- | --- |
| App dir (macOS / Windows) | `~/.agent-of-empires` | `~/.agent-of-empires-dev` |
| App dir (Linux) | `~/.config/agent-of-empires` | `~/.config/agent-of-empires-dev` |
| `tmux` session prefix | `aoe_` | `aoe_dev_` |
| `aoe serve` default port | `8080` | `8081` |

`cargo build --profile dev-release` counts as a release build for namespacing (shares app dir, tmux prefix, serve port); use the default `dev` profile for the isolated `-dev` namespace.

## Testing

```bash
cargo test       # Unit + integration tests
cargo fmt        # Format code
cargo clippy     # Lint
cargo check      # Fast type-check
```

Some integration tests require `tmux` to be available and will skip if it's not installed.

## Demo GIFs (rarely touched)

All three demos show the same flow on a different surface: create a real Claude Code session, send a message, and watch its status update in the sidebar. They are recorded against a live `aoe` (no mocks) by driving it with Playwright and converting the WebM to GIF via ffmpeg. Both recorders document their full setup recipe (isolated `$HOME`/`XDG_CONFIG_HOME` with Claude credentials, a scratch git repo, the `claude-agent-acp` adapter) at the top of the file.

**TUI demo** (`docs/assets/demo.gif`): `web/scripts/record-tui-demo.mjs` runs `aoe` inside [ttyd](https://github.com/tsl0922/ttyd) and creates a session that launches into live mode. Needs the profile's `default_attach_mode = "live_send"`.

**Web dashboard GIFs** (`docs/assets/web-{desktop,mobile}.gif`): `web/scripts/record-web-demo.mjs --viewport desktop|mobile --project <repo>` drives the structured (ACP) view against a real `aoe serve --no-auth`.
