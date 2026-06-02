# Web Dashboard Development

Contributor notes for building, running, and hacking on the web dashboard. End users do not need any of this; the dashboard ships in every release binary. See the [Web Dashboard guide](../guides/web-dashboard.md) for how to launch and use it.

## Building from source

The dashboard needs the `serve` Cargo feature and Node.js/npm:

```bash
cargo build --release --features serve
```

The build automatically runs `npm install && npm run build` in the `web/` directory to compile the React frontend. The output is embedded in the binary, so there are no separate files to deploy. A plain `cargo build` (without `--features serve`) needs no JS tooling and produces a TUI-only binary.

## One-command development

On Unix, `cargo xtask dev` is the fastest inner loop. It builds the serve binary, then runs `aoe serve` (port 8081) and the Vite dev server (port 5173, with HMR) together, pointing Vite at the backend via `VITE_PROXY` so `/api` and the `/sessions/*/ws` relays resolve:

```bash
cargo xtask dev
# Open http://localhost:5173
# Ctrl-C stops both processes
```

## Manual frontend development

If you prefer to run the pieces by hand, the React frontend lives in `web/`:

```bash
cd web
npm install
npm run dev     # Vite dev server with HMR on port 5173
```

For API/WebSocket requests, run the Rust server simultaneously:

```bash
cargo run --features serve -- serve
```

To work on the frontend against a "production" backend instead of a local build, set `VITE_PROXY` (shell env or `web/.env`) to that `aoe serve` origin, including a non-cargo install on a custom port, and the dev server forwards `/api` and `/sessions/*/ws` (terminal + cockpit) there:

```bash
VITE_PROXY=http://localhost:50106 npm run dev
```

Without it the dev server behaves as before; HMR is unaffected either way.

## Architecture

The server embeds an axum web server that serves the React frontend and provides:

- REST API for session listing and control (`/api/sessions`); see the [HTTP API Reference](../api.md) for the orchestration endpoints (`send`, `output`)
- WebSocket PTY relay for terminal streaming (`/sessions/:id/ws`)
- Token-based authentication via cookie, query parameter, or WebSocket protocol header
- Rate limiting, token rotation, and device tracking
- Security headers (X-Frame-Options, Referrer-Policy)

Each terminal connection spawns `tmux attach-session` inside a PTY and relays the raw byte stream bidirectionally over WebSocket. This gives the browser a real terminal experience identical to SSH, and is why sessions survive browser crashes and network drops.
