# Web Dashboard Guidelines

> `CLAUDE.md` links to this file. Root rules remain in `../AGENTS.md`.

## Commands

```sh
cargo build --features web
cargo xtask dev
cd web
npm run format:check
npm run lint
npx tsc -b
```

`cargo xtask dev` runs the debug server on 8081 and Vite on 5173. A build
without `--features web` needs no JavaScript tooling. Use `npm run format` to fix oxfmt output;
Prettier is not used.

## Tests

- Vitest with RTL and MSW: component logic and request payloads.
- Mocked Playwright: browser-only behavior such as focus, keyboard, drag and
  drop, touch, and viewports.
- Live Playwright: backend persistence, auth, sessions, tmux, git, read-only
  behavior, and structured-view round trips.

Run `npm run test:unit` for Vitest,
`npx playwright test --config=playwright.config.ts` for mocked browser tests,
and `npx playwright test --config=playwright.live.config.ts` for live tests.
Live tests use `tests/helpers/aoeServe.ts`, which gives each test an isolated
home, tmux socket, and port. Use its existing fixtures instead of launching a
shared server.

Any user-facing change to auth, session creation, settings, profiles, sessions,
sidebar, diff, notifications, directory browsing, devices, git clone,
connectivity, or read-only behavior must update
`tests/coverage-matrix.json` and its applicable test. Copy-only or styling
changes may use a deferred entry with a reason and linked issue.
