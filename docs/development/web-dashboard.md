# Web Dashboard Development

Contributor notes for hacking on the web dashboard. End users do not need any of this; the dashboard ships in every release binary. See the [Web Dashboard guide](../guides/web-dashboard.md) for launching and using it.

Build, run, and the `cargo xtask dev` inner loop (Vite + `aoe serve` with HMR, `--watch` auto-rebuild) are covered in [Development](../development.md). The dashboard needs the `serve` Cargo feature plus Node.js/npm; the build runs `npm install && npm run build` in `web/` and embeds the output in the binary, so there is nothing separate to deploy. A plain `cargo build` (no `serve`) needs no JS tooling.

## Manual frontend loop

The React frontend lives in `web/`. To run the pieces by hand instead of `cargo xtask dev`:

```bash
cd web && npm install && npm run dev    # Vite + HMR on :5173
cargo run --features serve -- serve     # backend, separate shell
```

To develop the frontend against an already-running "production" backend (e.g. a non-cargo install on a custom port), point `VITE_PROXY` (shell env or `web/.env`) at that `aoe serve` origin; the dev server forwards `/api` and `/sessions/*/ws` (terminal + structured view) there. HMR is unaffected either way.

```bash
VITE_PROXY=http://localhost:50106 npm run dev
```

## Architecture

The `serve` feature embeds an axum server that serves the React bundle and provides: the REST API (`/api/sessions`, plus the orchestration endpoints in the [HTTP API Reference](../api.md)), a WebSocket PTY relay (`/sessions/:id/ws`), token auth (cookie / query param / WS protocol header) with rate limiting, token rotation, and device tracking, and security headers.

Each terminal connection spawns `tmux attach-session` inside a PTY and relays the raw byte stream bidirectionally over the WebSocket. That gives the browser an SSH-grade terminal, and is why sessions survive browser crashes and network drops.
