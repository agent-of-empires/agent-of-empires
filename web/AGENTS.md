# Web Dashboard Guidelines

> `CLAUDE.md` is a symlink to this file. Do not edit `CLAUDE.md` directly; edit `AGENTS.md` instead.

Loaded when working with files under `web/`. Repo-wide rules live in the root
`AGENTS.md`.

## Build and run

- Installable as a PWA ("Install Agent of Empires" in Chrome; "Add to Home Screen" on iOS).
- Build: `cargo build --features serve` (build.rs runs `npm install && npm run build` in `web/` when inputs change).
- Run: `aoe serve --host 0.0.0.0` (token-based auth by default).
- Frontend dev: `cargo xtask dev` (Unix) builds the serve binary, then runs `aoe serve` (8081) and the Vite dev server (5173, HMR) together, pointing Vite at the backend via `VITE_PROXY` so `/api` and the `/sessions/*/ws` relays resolve; open `:5173`, Ctrl-C stops both. Or run them by hand: `cd web && npm run dev` plus a separate `cargo run --features serve -- serve`.
- Web checks (CI gates all three on any `web/` change): `cd web && npm run format:check` (oxfmt, NOT prettier; `npm run format` to fix), `npm run lint` (ESLint), and `npx tsc -b` (typecheck, also part of `npm run build`). ESLint and tsc do not catch formatting; run oxfmt explicitly.
- TUI-only `cargo build` (without `--features serve`) needs no JS tooling.

## Playwright Tests

Two suites under `web/`:

- **Mocked**: `web/tests/*.spec.ts`, run via `cd web && npx playwright test --config=playwright.config.ts`. Uses `page.route()` to stub `/api/*` responses; serves the production Vite bundle through `vite preview` on port 4173. Fast and deterministic; for UI logic that does not depend on real backend state.
- **Live**: `web/tests/live/*.spec.ts`, run via `cd web && npx playwright test --config=playwright.live.config.ts`. Each test spawns a real `aoe serve` subprocess against an isolated `HOME` via the harness in `web/tests/helpers/aoeServe.ts`. Two workers, `workerIndex`-based port allocation, `TMUX_TMPDIR` per test. For flows that depend on backend persistence, auth, sessions, tmux, git, read-only, or structured view.

When deciding which suite to use:

- Backend, persistence, auth, session, tmux, git, read-only, or structured-view round-trip flows belong in **live Playwright**.
- Request-payload permutations (does control X emit the right JSON keys) belong in **Vitest + RTL + MSW** under `web/src/**/__tests__/`. See `web/src/components/settings/__tests__/SoundSettings.test.tsx` as the canonical example.
- Browser-specific behavior not practical in Vitest (focus, keyboard, drag-drop, modal escape, mobile viewport, touch events) belongs in **mocked Playwright**.

**Mandate:** any PR that changes a user-facing dashboard flow under auth, wizard / session creation, settings, profiles, sessions / sidebar, right panel / diff / notifications, directory browser, devices, git clone, connectivity, or read-only behavior must update `web/tests/coverage-matrix.json` and add or modify the appropriate test. CI fails on a missing matrix entry via `web/tests/validate-coverage-matrix.mjs`. Pure styling or copy-only changes may add a `kind: "deferred"` entry with a `reason` and a linked issue.

Full recipe, harness API, fake-ACP-agent details, and the coverage / test-analytics / bundle-analysis CI plumbing live in `docs/development/playwright.md`.

**Legacy mobile/touch recipe (still applies for the mocked specs under `tests/`):**

1. Shim a fake tool on `$PATH` so `aoe add --cmd claude` creates a live tmux session (the live harness already does this for you).
2. Emulate mobile with `devices['iPhone 13']` so `pointer: coarse` matches.
3. Spy on PTY bytes by patching `WebSocket.prototype.send` in an `addInitScript` and pushing into `window.__WS_SENT__`.
4. Synthesize multi-touch via `page.evaluate` dispatching raw `new TouchEvent(...)` on `.xterm`; Playwright's `page.touchscreen` is single-finger only.

**Gotcha:** synthetic touchmove events fire back-to-back with Δt≈1ms, which blows up any `Δpx / Δtime` velocity calculation. Cap velocity and per-frame emit counts, or a real device will look sane while the e2e produces runaway momentum (or vice-versa).
