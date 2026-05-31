# Web Dashboard

The web dashboard lets you monitor and interact with agent sessions from any browser, including your phone, tablet, or another computer. It runs as an embedded web server inside the `aoe` binary.

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

## Features

- **Session list** with live status updates (Running, Waiting, Idle, Error)
- **Live terminal** via PTY relay, full terminal experience with all key sequences
- **Stop/restart** sessions from the browser
- **Mobile-responsive** layout (sidebar collapses on small screens)
- **Multi-profile** support (shows sessions from all profiles)
- **Connected Devices** view in Settings > Security
- **Push notifications** on Waiting / Idle / Error transitions, with per-session overrides ([guide](push-notifications.md))
- **First-run tutorial** highlighting the major UI regions and their shortcuts (see below)

### First-run tutorial

The first time you open the dashboard in a browser, an interactive walkthrough launches automatically and highlights the major regions: the command bar, the workspace sidebar, how to start a session, settings, and (inside a session) the diff panel and composer. Each step lists the keyboard shortcuts that apply to it, and every step has a **Skip** button so you can dismiss the whole tour in one click.

Completing or skipping the tour records that you have seen it (a per-browser `aoe-tour-seen` flag in `localStorage`), so it does not launch again on reload. Because the flag is per origin, a debug build on port 8081 and a release build on port 8080 each track it separately.

To replay it at any time, open the overflow menu (the three-dot **More options** button in the top bar) and choose **Show tutorial**. Re-triggering it adapts to where you are: on the dashboard it covers the dashboard regions; inside a session it also covers the composer, agent mode picker, and send/queue controls. The tutorial does not auto-launch on touch devices, where it is available only from the menu.

On phones (below the `md` breakpoint) the dashboard shows a single full-viewport pane rather than the desktop side-by-side split. The right-panel button in the top bar opens a picker that swaps the main pane between three views: the **Agent terminal**, the **Diff** (changed files and review), and the **Paired terminal** (host or container shell). A back chip in the top-left of the diff and paired views returns you to the agent terminal.

This replaces the earlier slide-in overlay, which left the paired terminal almost no room once the soft keyboard opened. Because each view now owns the whole viewport, the paired terminal handles the keyboard the same way the agent terminal does. The agent terminal and the paired shell stay alive in the background when you switch away, so their scrollback and focus are preserved. The desktop side-by-side split is unchanged.

### Terminal copy and scroll

The live terminal uses tmux for scrollback and selection, so copy and scroll
work together with no modifier keys:

- **Scroll** with the mouse wheel (or a two-finger swipe on touch) to move back
  through tmux scrollback. A "Back to live" control returns you to the bottom.
- **Select** by click-dragging across the text. Dragging upward past the top
  edge scrolls into the scrollback and extends the selection, so you can grab
  output taller than the window. Releasing the drag copies the selection to
  your system clipboard automatically; no Ctrl/Cmd+C needed.

Copy-to-clipboard relies on the browser Clipboard API, which is only available
in a secure context: pages served over HTTPS (the remote-access tunnel modes)
or over `http://localhost`. On a plain-HTTP LAN/VPN origin the browser blocks
clipboard writes, and the selection stays visible but is not copied. Firefox
is best-effort here because it does not support the asynchronous clipboard
write the terminal uses; Chromium and Safari copy reliably.

### Sidebar sort

By default the sidebar shows your manually-ordered list. Drag a row with a press-and-hold gesture to move it; the new order persists across browsers and devices via `workspace-ordering.json`.

To reorder whole projects, grab the drag handle on the left of a project/group header and drag it up or down. This sets an explicit group order instead of leaving project placement to be derived from whichever session sits highest. Unlike row ordering, the group order is per-browser (localStorage), not synced across devices. A project that appears after you have set an order slots in at the top. The Multi-repo and Scratch groups default to the bottom but are draggable too, so you can lift them anywhere; once dragged they hold their chosen spot. Group drag is disabled while a filter is active or while Recent activity sort is selected, since the order is computed in those cases.

A sort toggle next to the filter button in the sidebar header switches to **Recent activity** mode, which orders workspaces by the most recent of `last_accessed_at`, `idle_entered_at`, and `created_at` across each workspace's sessions, descending. Drag-to-reorder is disabled while Recent activity is selected, because the order is computed; the press-and-hold gesture does nothing in that mode.

The toggle's state is per-browser (localStorage), not synced across devices and not tied to your profile. Toggling back to manual restores the stored manual order and re-enables drag. The multi-repo group stays pinned at the bottom in both modes.

### Sidebar grouping: by repo or by group

A grouping toggle (the layers icon) next to the sort toggle switches the axis the sidebar organises sessions by:

- **By repo** (default) groups workspaces by their git repository, the original behavior.
- **By group** groups sessions by the user-defined group you assigned in the TUI rename dialog or with `aoe group move`, mirroring the TUI's group headers. Sessions with no group fall into an **Ungrouped** bucket pinned to the bottom. A session whose worktree hosts agents in different groups shows up under each of those groups.

The choice is per-browser (localStorage). Collapse state is tracked separately for each axis, so collapsing a group in **By group** does not collapse a repo in **By repo**. The group axis is read-only in v1: group rename, color, and drag-reorder live on the repo axis only, and groups themselves are not yet reorderable.

### Triage: pin, archive, snooze

The sidebar exposes three triage primitives via the right-click (long-press on touch) context menu on any session row:

- **Pin** floats the workspace to the top of the sidebar in every sort mode (manual and Recent activity). Pin is web-only and intentionally distinct from the TUI's favorite mark, which is a within-tier signal for the Attention sort. The web pin renders as a pushpin glyph next to the row title; the TUI favorite keeps its `*` star marker.
- **Archive** kills the session's tmux pane (or shuts down the cockpit worker for ACP-cockpit sessions) and sinks the row into the collapsible "Snoozed & archived" footer of its repo group. Sending a message to the row from the dashboard wakes it back into the live list automatically. Daemon restarts and the cockpit worker reconciler both skip archived sessions, so a row stays parked until you explicitly unarchive it.
- **Snooze** sinks the row into the same footer for a chosen duration. The menu offers the same eight presets as the TUI snooze dialog: 1h, 2h, 3h, 4h, 5h, 6h, 1d, 1w. The row wakes automatically when the timer expires; sending a message wakes it early.

Snooze and archive are mutually exclusive with pin: pinning a sunk row surfaces it, and archiving or snoozing a pinned row removes the pin. The three primitives can be mixed freely across concurrent surfaces (TUI, CLI, web), and the data layer enforces the mutual-exclusion rules in one place so peer writes cannot leave a row in a contradictory state.

The "Snoozed & archived" section sits at the very bottom of the sidebar and aggregates every sunk workspace across all repo groups. It is collapsed by default; clicking the header expands the list and remembers the choice in localStorage. Drag-to-reorder is disabled on pinned and sunk rows since their placement is computed.

In read-only mode (`aoe serve --read-only`) the three menu entries are hidden, matching the existing read-only gate on Delete.

## Architecture

The server embeds an axum web server that serves a React frontend and provides:

- REST API for session listing and control (`/api/sessions`); see the [HTTP API Reference](../api.md) for the orchestration endpoints (`send`, `output`)
- WebSocket PTY relay for terminal streaming (`/sessions/:id/ws`)
- Token-based authentication via cookie, query parameter, or WebSocket protocol header
- Rate limiting, token rotation, and device tracking
- Security headers (X-Frame-Options, Referrer-Policy)

Each terminal connection spawns `tmux attach-session` inside a PTY and relays the raw byte stream bidirectionally over WebSocket. This gives the browser a real terminal experience identical to SSH.

### Terminal WebSocket close codes

When the browser fails to reach a working terminal, the disconnect banner shows the close code returned by the server. Decoder ring:

| Code | Reason string             | Meaning                                                                                                                                              | Client behavior              |
| ---- | ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------- |
| 1001 | `server shutdown`         | Daemon is shutting down (SIGINT/SIGTERM).                                                                                                            | Retry with normal backoff.   |
| 1011 | `openpty_failed`          | Server could not allocate a PTY.                                                                                                                     | Retry with normal backoff.   |
| 1011 | `attach_spawn_failed`     | Server could not spawn the `tmux attach-session` child process.                                                                                      | Retry with normal backoff.   |
| 1011 | `pty_reader_failed`       | Server could not clone the PTY reader handle.                                                                                                        | Retry with normal backoff.   |
| 1011 | `pty_writer_failed`       | Server could not take the PTY writer handle.                                                                                                         | Retry with normal backoff.   |
| 1013 | `tmux_not_ready`          | Server polled `tmux has-session` / `list-panes` for up to 2s and the pane did not become attachable. Usually a benign warm-up on first session open. | Retry with normal backoff.   |
| 4001 | `pty_dead`                | PTY relay was running but the pane permanently exited. Continuing to retry would just hammer a dead session.                                         | Show "Click retry" banner.   |

The browser uses a fast-start retry ladder (200ms, 400ms, 800ms, 1.5s, 3s, 6s, 10s) so transient warm-up failures recover in well under 5 seconds end-to-end. Permanently dead panes short-circuit on 4001 and surface a manual reconnect button.

## Frontend development

The React frontend lives in `web/`:

```bash
cd web
npm install
npm run dev     # Vite dev server with HMR on port 5173
```

For API/WebSocket requests, run the Rust server simultaneously:

```bash
cargo run --features serve -- serve
```

The Vite dev server proxies API requests to the Rust server (configure in `vite.config.ts` if needed).
