# Web Dashboard

The web dashboard lets you monitor and interact with agent sessions from any browser, including your phone, tablet, or another computer. It runs as an embedded web server inside the `aoe` binary.

![The web dashboard on desktop: workspace sidebar, live agent terminal, and diff panel](../assets/web/dashboard.png)

## In this section

This page is the overview: how to run `aoe serve`, the access and auth modes, the security model, and PWA install. The rest of the surface has its own pages:

- **[Dashboard & workspaces](web/dashboard.md)**: the layout, status glyphs, the session-creation wizard, sidebar sort, triage (pin / archive / snooze), command palette, and the first-run tutorial.
- **[Terminal view](web/terminal.md)**: the agent and paired terminals, reconnect behavior, the WebSocket close-code reference, and read-only mode.
- **[Diff view](web/diff.md)**: reviewing changed files, the flat / tree file list, per-session base override, and inline review comments.
- **[Settings & profiles](web/settings.md)**: the settings tabs, profile picker, connected-device tracking, and step-up elevation.

Mobile and touch behavior is documented inline on each page, next to the surface it applies to.

## Availability

The web dashboard is included in all release binaries: [GitHub Releases](https://github.com/agent-of-empires/agent-of-empires/releases), the [quick install script](../installation.md#quick-install-recommended), and Homebrew (`brew install aoe`). No extra build steps needed, just run `aoe serve`.

## Building from source

If building from source, you need the `serve` Cargo feature and Node.js/npm:

```bash
cargo build --release --features serve
```

The build automatically runs `npm install && npm run build` in the `web/` directory to compile the React frontend. The output is embedded in the binary, so there are no separate files to deploy.

## Starting the server

```bash
# Localhost only (safe, default)
aoe serve

# Remote access over HTTPS (Tailscale Funnel if available, else Cloudflare quick tunnel)
aoe serve --remote

# Accessible from other devices on your LAN/VPN (HTTP, requires VPN)
aoe serve --host 0.0.0.0

# Run in background
aoe serve --daemon

# Open the printed URL in the default browser once the server is ready
aoe serve --open

# Read-only monitoring (no terminal input)
aoe serve --remote --read-only
```

The server prints a URL with an auth token:

```
aoe web dashboard running at:
  http://localhost:8080/?token=a1b2c3...
```

Open this URL in any browser to access the dashboard. The token is set as a cookie on first visit so you don't need to keep it in the URL.

`--open` is opt-in. It is suppressed when you also pass `--daemon` or `--remote`, when running over SSH (`SSH_CONNECTION` / `SSH_TTY` set), and on Linux/BSD with no `DISPLAY` / `WAYLAND_DISPLAY`.

## Retrieving the live URL

In `--remote` mode the auth token rotates every 4 hours, so a URL captured at startup eventually stops working. Use `aoe url` to print the current dashboard URL of a running daemon:

```bash
# Print the primary URL with the live token
aoe url

# Print every labeled URL (Tailscale / LAN / localhost), tab-separated
aoe url --all

# Print only the auth token (useful for scripted login flows)
aoe url --token-only
```

`aoe url` exits non-zero if no daemon is running.

In `--remote` mode, a QR code is also printed for easy phone pairing.

## Remote access

The `--remote` flag is the recommended way to access the dashboard from your phone or another device:

```bash
aoe serve --remote
```

aoe picks a transport automatically in this order:

### 1. Tailscale Funnel (preferred when available)

If `tailscale` is on the host's PATH and the daemon is logged in, aoe runs `tailscale funnel --bg --yes <port>` (the Tailscale 1.52+ single-command Funnel syntax) and exposes the dashboard at your stable `https://<machine>.<tailnet>.ts.net` URL. No domain, no Cloudflare account, no rotating URLs. **This is the only option where a PWA installed on your phone keeps working across server restarts** (the URL is stable).

Setup (two one-time gates; aoe surfaces the fix if either is missing):
1. Install Tailscale on the host ([tailscale.com/download](https://tailscale.com/download))
2. `tailscale up`
3. **Enable the Funnel feature for your tailnet** (tailnet-wide switch): [login.tailscale.com/f/funnel](https://login.tailscale.com/f/funnel). When this isn't enabled, `tailscale funnel` prints a node-specific activation URL; aoe detects that URL in stderr and bails in seconds with the link instead of timing out.
4. **Grant the `funnel` nodeAttr to this node** in your tailnet ACL: [login.tailscale.com/admin/acls/file](https://login.tailscale.com/admin/acls/file). A default rule like `{ "target": ["autogroup:member"], "attr": ["funnel"] }` works for personal tailnets; if your node is tagged, target the tag instead (`autogroup:member` excludes tagged devices).
5. `aoe serve --remote`

Caveat: `aoe serve --remote` runs `tailscale funnel --bg --yes <port>`, which configures port 443 to proxy to the dashboard. If you already have a non-loopback service on port 443 of this node's Funnel config (your own webapp pointing at a tailnet IP, a remote service), aoe refuses to start rather than silently replace it. A stale loopback config from a prior aoe run is fine, aoe overwrites that cleanly. Clear any conflict with `tailscale funnel reset` (the Error dialog offers this as `[R]`) and re-run, or pass `--no-tailscale` to use Cloudflare instead.

### 2. Named Cloudflare tunnel

Stable hostname on your own Cloudflare-managed domain. Takes precedence over Tailscale auto-detection when you pass the flags:

```bash
# One-time setup
cloudflared tunnel create my-tunnel
# Add a CNAME record: aoe.example.com -> <tunnel-id>.cfargotunnel.com

# Run with stable URL
aoe serve --remote --tunnel-name my-tunnel --tunnel-url aoe.example.com
```

### 3. Cloudflare quick tunnel (fallback)

Zero-config but the URL rotates on every restart. Fine for one-off remote sessions, **bad for installed PWAs**: the home-screen app is bound to the URL it was installed from, so every restart costs you a delete-and-reinstall.

```bash
aoe serve --remote
```

Requires `cloudflared` on the host:
- macOS: `brew install cloudflared`
- Linux: `sudo apt install cloudflared`
- Other: [Cloudflare's downloads page](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/)

aoe prints a notice when it falls back to this path so you don't accidentally install a PWA from a rotating URL.

## Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | 8080 | Port to listen on |
| `--host` | 127.0.0.1 | Bind address. Use `0.0.0.0` for LAN/VPN access |
| `--auth` | `token` | Auth mode: `token` (URL token, default), `passphrase` (passphrase login wall only), `none` (no auth, loopback only unless `--behind-proxy`) |
| `--passphrase` | | Passphrase for the login wall. Valid with `--auth=token` (token + passphrase) and `--auth=passphrase`. Also reads `AOE_SERVE_PASSPHRASE` |
| `--behind-proxy` | off | Server sits behind an external reverse proxy that terminates TLS. Sets cookies as `; Secure` and trusts `X-Forwarded-For` / `cf-connecting-ip` from loopback peers; does NOT spawn a tunnel |
| `--no-auth` | off | Alias for `--auth=none` (kept for backwards compatibility) |
| `--remote` | off | Expose over HTTPS tunnel (Tailscale Funnel if available, else Cloudflare quick tunnel) |
| `--tunnel-name` | | Use a named Cloudflare tunnel (requires `--remote`; overrides Tailscale auto-detection) |
| `--no-tailscale` | off | Skip Tailscale Funnel auto-detection and use Cloudflare (requires `--remote`) |
| `--tunnel-url` | | Hostname for a named tunnel (requires `--tunnel-name`) |
| `--read-only` | off | View terminals but cannot send keystrokes |
| `--daemon` | off | Fork to background and detach from terminal |
| `--stop` | | Stop a running daemon |

### Auth mode matrix

| Mode | Token URL | Passphrase wall | Use case |
|------|-----------|-----------------|----------|
| `--auth=token` (default) | required | optional (`--passphrase`) | Standard local / VPN / Tailscale deployments |
| `--auth=passphrase --passphrase X` | none | required | Reverse-proxy deployments where pasting a token URL on mobile is too high friction |
| `--auth=none` (alias `--no-auth`) | none | none | Localhost-only quick testing |

Notes:

- `--auth=passphrase` and `--auth=none` on a non-loopback bind require `--behind-proxy`. The flag asserts that an upstream reverse proxy terminates TLS and forwards the client IP. Without it, reduced-auth modes refuse to bind to a routable address.
- `--auth=passphrase` requires `--passphrase <VALUE>` (or `AOE_SERVE_PASSPHRASE`) since the passphrase becomes the only human gate.
- `--auth=none --passphrase X` is rejected explicitly; the previous silent acceptance of `--no-auth --passphrase` was a footgun. Use `--auth=passphrase` if the passphrase wall is what you want.
- `--remote` is incompatible with `--auth=none` and `--auth=passphrase`; the public tunnel mandates both token auth and a passphrase.

### Behind a reverse proxy

When TLS is terminated by an external reverse proxy (Traefik, nginx, Caddy) that forwards traffic to `aoe serve` on loopback (often through an SSH reverse tunnel), use `--behind-proxy` so cookies carry `; Secure` and the rate limiter keys requests by the real client IP:

```bash
# Loopback bind, passphrase login wall, TLS terminated upstream.
aoe serve \
  --host 127.0.0.1 --port 42041 \
  --auth=passphrase --passphrase "$AOE_PASSPHRASE" \
  --behind-proxy
```

The upstream proxy must set `X-Forwarded-For` (or `cf-connecting-ip`); aoe reads the last value as the client IP. The trust check fires only when the socket peer is loopback, so a misconfigured upstream that lets requests reach aoe directly cannot spoof the IP.

## Security

**The web dashboard exposes terminal access.** Anyone who authenticates can send keystrokes to your agent sessions, which run as your user.

### Authentication

- **Token auth** (`--auth=token`, default): A random 256-bit token is generated on startup and stored at `~/.config/agent-of-empires/serve.token` (Linux) or `~/.agent-of-empires/serve.token` (macOS). The token is passed via URL on first visit, then stored as an `HttpOnly; SameSite=Strict` cookie.
- **Passphrase wall** (`--auth=passphrase`, or combined with token via `--passphrase`): An argon2-hashed passphrase gates `/login`. Sessions are bound to a per-device secret stored in the client's `localStorage`; a leaked session cookie alone is insufficient.
- **Rate limiting:** 5 failed login attempts from an IP trigger a 15-minute lockout. Uses `Cf-Connecting-IP` / `X-Forwarded-For` from loopback peers (covers `--remote` tunnel mode and `--behind-proxy` reverse-proxy mode) to prevent IP spoofing.
- **Token rotation:** In `--remote` mode, the token rotates every 4 hours with a 5-minute grace period for active sessions.
- **Device tracking:** Connected devices (IP, browser, last seen) are visible in Settings > Security.
- **Step-up elevation:** A "Confirm passphrase" prompt appears on writes whose payload can plant code for the next session spawn: the `sandbox` and `worktree` sections, and dangerous `session` fields (`agent_command_override`, `agent_extra_args`, `extra_env`, `custom_agents`, `agent_detect_as`). Confirmation lasts 15 minutes. User-preference writes (theme, sound, updates, notification toggles, logging filter, profile description, and safe session fields like `yolo_mode_default`) save without the prompt; saving a theme should not feel like signing in again.

### Security headers

The server sets `X-Frame-Options: DENY` (prevents clickjacking), `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer` (prevents token leaking via Referer headers).

### Safe usage patterns

- **Localhost** (`aoe serve`): Same security as the TUI. Fine.
- **Remote via tunnel** (`aoe serve --remote`): Encrypted via HTTPS. Recommended for phone access.
- **Over Tailscale/WireGuard** (`aoe serve --host 0.0.0.0`): The VPN encrypts traffic.
- **Behind a reverse proxy** (`aoe serve --auth=passphrase --passphrase ... --behind-proxy`): TLS terminated upstream by Traefik / nginx / Caddy. Passphrase is the only human gate.
- **Read-only** (`aoe serve --remote --read-only`): Monitor sessions without input capability.

### Dangerous

- `aoe serve --host 0.0.0.0` on public WiFi without a VPN: traffic is unencrypted HTTP
- `aoe serve --auth=none --host 0.0.0.0` (or alias `--no-auth --host 0.0.0.0`): blocked (refuses to start without `--behind-proxy`)
- `aoe serve --auth=none --remote` or `--auth=passphrase --remote`: blocked (refuses to start)

## Installing as a PWA

The dashboard supports Progressive Web App (PWA) installation for an app-like experience:

**macOS (Chrome):** Three-dot menu > "Install Agent of Empires" -- creates a standalone window with a Dock icon.

**macOS (Safari):** File > Add to Dock.

**iOS:** Share > Add to Home Screen.

**Android:** Chrome will prompt "Add to Home Screen" or show an install banner.

The PWA requires the server to be running. Use `--daemon` to keep it running in the background:

```bash
aoe serve --daemon
# Server runs in background, prints PID
# Stop with: aoe serve --stop
```

### Shutdown behavior

`Ctrl-C` on a foreground `aoe serve`, and `aoe serve --stop` against a daemon, both exit within ~5 seconds even with open dashboard tabs. Live cockpit and terminal WebSocket clients receive a close frame with code `1001` ("going away") so the browser logs a clean reason and skips its transient-error reconnect backoff for one cycle. The reconnect resumes normally once a fresh `aoe serve` is running. A 5-second hard cap acts as a safety net: if any handler fails to honor the shutdown signal, the process still exits and emits `WARN shutdown: graceful shutdown exceeded grace window, forcing exit`.

## Architecture

The server embeds an axum web server that serves a React frontend and provides:

- REST API for session listing and control (`/api/sessions`); see the [HTTP API Reference](../api.md) for the orchestration endpoints (`send`, `output`)
- WebSocket PTY relay for terminal streaming (`/sessions/:id/ws`)
- Token-based authentication via cookie, query parameter, or WebSocket protocol header
- Rate limiting, token rotation, and device tracking
- Security headers (X-Frame-Options, Referrer-Policy)

Each terminal connection spawns `tmux attach-session` inside a PTY and relays the raw byte stream bidirectionally over WebSocket. This gives the browser a real terminal experience identical to SSH.

The terminal disconnect banner surfaces a WebSocket close code when it can't reach a working pane. The decoder ring for those codes lives on the [Terminal view](web/terminal.md#terminal-websocket-close-codes) page.

## Frontend development

The React frontend lives in `web/`. One command from the repo root builds the
serve-enabled binary, then runs the backend and the Vite dev server together:

```bash
cargo xtask dev
```

This starts `aoe serve --no-auth` on port 8081 and the Vite dev server on
[http://localhost:5173](http://localhost:5173), pointing Vite at the backend via
`VITE_PROXY`. Open the `:5173` URL (not `:8081`): Vite serves the app with hot
module reload and proxies `/api` and the `/sessions/*/ws` relays to the backend,
so edits to `.tsx` files reload instantly while API and terminal traffic keep
working. One Ctrl-C stops both processes. Override ports with `--serve-port` /
`--web-port`. Unix only.

To run the two halves by hand instead:

```bash
cd web
npm install
npm run dev     # Vite dev server with HMR on port 5173
```

For API/WebSocket requests, run the Rust server simultaneously:

```bash
cargo run --features serve -- serve
```

If you are making only frontend changes and want to work on the local frontend against a "production" backend instead of a local build, set `VITE_PROXY` (shell env or `web/.env`) to that `aoe serve` origin, including a non-cargo install on a custom port, and the dev server forwards `/api` and `/sessions/*/ws` (terminal + cockpit) there: `VITE_PROXY=http://localhost:50106 npm run dev`. Without it the dev server behaves as before; HMR is unaffected either way.
