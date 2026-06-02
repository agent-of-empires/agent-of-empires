# Development

## Building

```bash
cargo build                    # Debug build
cargo build --release          # Release build (with LTO)
cargo build --profile dev-release  # Optimized build without LTO (faster compile)
```

The release binary is at `target/release/aoe`.

The web dashboard needs the `serve` feature and Node.js: `cargo build --release --features serve`. See [Web Dashboard Development](development/web-dashboard.md).

## Running

```bash
cargo run --release            # Run from source
AGENT_OF_EMPIRES_DEBUG=1 cargo run  # Debug logging (writes to debug.log in app data dir)
AOE_LOG_LEVEL=trace cargo run        # Pick the log level explicitly
AOE_ACP_TRACE=1 cargo run            # Plus raw ACP JSON-RPC firehose; useful for
                                     # verifying sub-agent linkage
                                     # (`_meta.claudeCode.parentToolUseId` round-trip)
                                     # and other adapter-side _meta fields. Cockpit
                                     # also logs a `cockpit.acp` debug line whenever
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
both. Ports are overridable with `--serve-port` / `--web-port`. See the
[web dashboard guide](guides/web-dashboard.md#frontend-development) for the
manual two-shell alternative.

### Dev namespace

Debug builds use an isolated namespace so a local `cargo run` shares no
state with an installed release `aoe`. Run them side-by-side without
collisions on sessions, settings, the tmux server, or `aoe serve`.

| | Release | Debug (`cargo run`) |
| --- | --- | --- |
| App dir (macOS / Windows) | `~/.agent-of-empires` | `~/.agent-of-empires-dev` |
| App dir (Linux) | `~/.config/agent-of-empires` | `~/.config/agent-of-empires-dev` |
| `tmux` session prefix | `aoe_` | `aoe_dev_` |
| `aoe serve` default port | `8080` | `8081` |

`debug.log` lives inside the app dir, so it is isolated automatically. Debug builds start with an empty namespace on first run, so
nothing migrates from your real `~/.agent-of-empires`. Wipe dev state any
time with `rm -rf ~/.agent-of-empires-dev` (or the Linux XDG equivalent);
release data is untouched.

`cargo build --profile dev-release` is treated as a release build for
namespace purposes, so it shares the app dir, tmux prefix, and `aoe serve`
port with an installed release `aoe`. Use the default `dev` profile when
you want the isolated `-dev` namespace.

## Testing

```bash
cargo test       # Unit + integration tests
cargo fmt        # Format code
cargo clippy     # Lint
cargo check      # Fast type-check
```

Some integration tests require `tmux` to be available and will skip if it's not installed.

## Generating the Demo GIF

The demo GIF in the docs is created using [VHS](https://github.com/charmbracelet/vhs).

```bash
# Install VHS (macOS)
brew install vhs

# Build aoe with the serve feature so the tape can exercise remote access
cargo build --release --features serve

# Generate the GIF (from repo root). The tape cleans its own profile
# (`~/.config/agent-of-empires/profiles/demo` on Linux,
# `~/.agent-of-empires/profiles/demo` on macOS) and demo scratch repo.
vhs assets/demo.tape
```

This writes `docs/assets/demo.gif`. The tape runs `aoe -p demo` so your real profile is untouched.

## Generating the Web Dashboard GIFs

`docs/assets/web-desktop.gif` and `docs/assets/web-mobile.gif` are recorded against a real `aoe serve` backend with real opencode sessions, no mocks. The recorder lives in `web/scripts/record-web-demo.mjs`.

```bash
# 1. Build with the serve feature.
cargo build --release --features serve

# 2. Set up an isolated profile with two scratch git repos and two opencode sessions.
SANDBOX=/tmp/aoe-webdemo
rm -rf "$SANDBOX"
mkdir -p "$SANDBOX/home/.config" "$SANDBOX/projects/api-server" "$SANDBOX/projects/web-app"
for d in "$SANDBOX/projects/"*; do
  (cd "$d" && git init -q && git config user.email t@t && git config user.name t \
    && touch README.md && git add . && git commit -q -m init)
done
export HOME=$SANDBOX/home XDG_CONFIG_HOME=$SANDBOX/home/.config
target/release/aoe add "$SANDBOX/projects/api-server" -t "API Server" -c opencode
target/release/aoe add "$SANDBOX/projects/web-app"    -t "Web App"    -c opencode

# 3. Start the server (no auth, localhost only).
target/release/aoe serve --host 127.0.0.1 --port 8181 --no-auth &

# 4. Record both viewports. Each run drives the live dashboard with Playwright,
#    captures WebM, and converts to GIF with ffmpeg.
node web/scripts/record-web-demo.mjs --viewport desktop --port 8181
node web/scripts/record-web-demo.mjs --viewport mobile  --port 8181
```

opencode's free tier needs no credentials, so the sessions produce real LLM responses inside the recording. Reset between runs by killing tmux (`HOME=$SANDBOX/home tmux kill-server`) so each session starts fresh.
