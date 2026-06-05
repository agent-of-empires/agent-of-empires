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

To fix this the committed `.cargo/config.toml` sets
[kache](https://github.com/kunobi-ninja/kache) as the rustc wrapper. kache is a
content-addressed build cache: it compiles each crate once, stores the artifact
in a shared local store (`~/Library/Caches/kache` on macOS, `~/.cache/kache` on
Linux), and **shares it into each worktree's `target/` without copying** by
reflinking it (a copy-on-write clone, on APFS/btrfs/xfs) or hardlinking it (on
filesystems without reflink support, such as ext4). The first build of a given
`Cargo.lock` populates the store; every later build, in any worktree, restores
the dependency artifacts instead of recompiling them. Each worktree still
produces its own `target/debug/aoe` binary, and the shared blocks mean one
physical copy of each dependency artifact backs all worktrees, so disk usage
drops too.

Because the wrapper is committed, **kache must be installed to build locally.**
Install it with a prebuilt binary (do not `cargo install` it from inside this
repo, see the note below):

```bash
cargo binstall kache         # prebuilt binary, recommended
# or
mise use -g github:kunobi-ninja/kache@latest
```

You do not need to set anything else; the wrapper and `CARGO_INCREMENTAL`
(disabled, kache and incremental compilation are mutually exclusive) come from
the committed `.cargo/config.toml`. Build exactly as before:

```bash
cargo build --all-features   # unchanged; rustc now routes through kache
```

Watch cache hits and deduplicated bytes live with `kache monitor`, or print a
non-interactive summary with `kache stats`.

### Opting out

If you do not want kache (for example a one-off contribution, or you prefer
sccache), disable the wrapper in your shell. The empty value overrides the
committed config key, exactly as CI does:

```bash
export CARGO_BUILD_RUSTC_WRAPPER=""        # plain rustc, no cache
# or point it at another wrapper you have installed:
export CARGO_BUILD_RUSTC_WRAPPER="sccache" # time-only cache, no disk dedup
```

CI, the Nix build, and release builds all set `CARGO_BUILD_RUSTC_WRAPPER=""` so
they never require kache and the shipped release binaries are compiled by plain
rustc.

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
- **Bootstrap.** Do not run `cargo install --git ... kache` from inside this
  repo: cargo would read the committed `rustc-wrapper = "kache"` and try to use
  kache to compile kache. Use the prebuilt `cargo binstall` / `mise` install
  above, or run `cargo install` from a directory outside the repo.

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
`/sessions/*/ws` relays to the backend (via `VITE_PROXY`). One Ctrl-C stops
both. Ports are overridable with `--serve-port` / `--web-port`. See
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
restart drops all live terminal and cockpit WebSocket connections.

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

**TUI demo** (`docs/assets/demo.gif`): uses [VHS](https://github.com/charmbracelet/vhs). `brew install vhs`, `cargo build --release --features serve`, then `vhs assets/demo.tape` from the repo root. The tape runs `aoe -p demo` and cleans its own demo profile, so your real profile is untouched.

**Web dashboard GIFs** (`docs/assets/web-{desktop,mobile}.gif`): recorded against a real `aoe serve` with real opencode sessions (no mocks) by `web/scripts/record-web-demo.mjs`, which drives the live dashboard with Playwright and converts WebM to GIF via ffmpeg. The recipe (build with `--features serve`, set up an isolated `$HOME`/`XDG_CONFIG_HOME` profile with two scratch git repos + two `opencode` sessions, `aoe serve --no-auth`, then run the recorder per viewport) is at the top of that script. opencode's free tier needs no credentials, so recordings get real LLM responses; reset between runs with `HOME=$SANDBOX/home tmux kill-server`.
