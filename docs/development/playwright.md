# Playwright + Vitest testing for the web dashboard

This document is the long-form reference for the web suite. The short version lives in `AGENTS.md` under "Web Dashboard Playwright Tests". Read the short version first.

## The two suites

| Suite | Config | Where | Speed | When to use |
|---|---|---|---|---|
| Mocked Playwright | `web/playwright.config.ts` | `web/tests/*.spec.ts` | Fast | UI logic that does not depend on real backend state |
| Live Playwright | `web/playwright.live.config.ts` | `web/tests/live/*.spec.ts` | Slower (real cargo binary + tmux) | Backend, persistence, auth, sessions, structured view, read-only |
| Vitest + RTL + MSW | `web/vite.config.ts` (`test` block) | `web/src/**/__tests__/`, `web/src/**/*.test.{ts,tsx}` | Very fast | Request-payload permutations, local UI state |

Run them:

```bash
cd web
npx playwright test --config=playwright.config.ts        # mocked
npx playwright test --config=playwright.live.config.ts   # live
npm run test:unit                                        # vitest
npm run test:unit -- --coverage                          # vitest with v8 coverage
```

## Picking the right tool

Decision tree:

1. Does the test need real backend persistence, auth, tmux, git, or structured view? → live Playwright.
2. Does the test need a browser-specific behavior (focus, keyboard, drag-drop, modal escape, touch event) with no real backend? → mocked Playwright.
3. Otherwise → Vitest + React Testing Library, with MSW if you need to assert request payloads.

Heuristics:

- "Every settings control emits the right JSON keys" → Vitest, not Playwright. Looping 30 controls through a live server is slow without adding signal.
- "Theme persists across page reload" → live Playwright. The test point is the backend round-trip.
- "The wizard's review step lets me edit the title inline and Escape cancels" → mocked Playwright. No backend needed; browser-specific Escape handling matters.
- "POST /api/settings with body X returns 200, the value persists" → both: Vitest contract for the payload shape, live Playwright for one representative round-trip.

## Live harness (`web/tests/helpers/aoeServe.ts`)

`spawnAoeServe()` boots a real `aoe serve` subprocess against a per-test isolated filesystem root. Three fixtures wrap it in `liveTest.ts`:

```ts
import { test, expect, seedAuth } from "../helpers/liveTest";

test("dashboard loads", async ({ serve, page }) => {
  await page.goto(serve.baseUrl);
  await expect(page.getByRole("heading", { name: "Sessions" })).toBeVisible();
});

test("login flow", async ({ servePassphrase, page }) => {
  await page.goto(servePassphrase.baseUrl);
  await page.locator("input#passphrase").fill(servePassphrase.passphrase!);
  await page.locator("button[type=submit]").click();
});

test("read-only blocks new sessions", async ({ serveReadOnly }) => {
  const res = await fetch(`${serveReadOnly.baseUrl}/api/sessions`, {
    method: "POST",
    body: "...",
  });
  expect(res.status).toBe(403);
});
```

Fixtures:

- `serve` : `aoe serve --no-auth`. Default for backend round-trip flows.
- `servePassphrase` : `aoe serve --passphrase aoe-e2e-fixed-passphrase`. The harness mints a session cookie via `POST /api/login` and exposes it as `handle.sessionCookie`. Specs that drive auth from the browser side use `seedAuth(page, handle)` to inject cookie + binding secret before the first navigation.
- `serveToken` : `aoe serve --auth=token`. The harness reads the daemon-written `serve.token` from the isolated app dir and exposes the value as `handle.authToken` plus its on-disk path as `handle.tokenFile`. Rotation-aware specs call `spawnAoeServe` directly with `tokenLifetimeSecs` / `tokenGraceSecs` overrides; both env vars are debug-build only (`AOE_TEST_TOKEN_LIFETIME_SECS`, `AOE_TEST_TOKEN_GRACE_SECS`) and ignored in release.
- `serveReadOnly` : `aoe serve --no-auth --read-only`.
- `serveAcp` : like `serve` but the fake-ACP agent (see below) is on `$PATH` as `claude`, `claude-agent-acp`, and `aoe-agent`, (the structured view is the default, so no enable step is needed). The `claude-agent-acp` name matters because the structured view supervisor resolves the `claude` tool key through `AgentRegistry` to command `claude-agent-acp`, not `claude`; without that shim the supervisor would fall through to the system-installed adapter and fail with "Authentication required" on the first prompt.

Isolation per test:

- Fresh `mkdtemp` for `HOME`, with `XDG_CONFIG_HOME`, `TMPDIR`, `TMUX_TMPDIR`, and `bin/` as subdirs (all `0700`).
- Port: `5200 + workerIndex*100 + parallelIndex + attempt*7`. Five retries on bind failure.
- Fake `claude` shim in `home/bin/claude` (`exec tail -f /dev/null`). Structured view fixture overrides with the fake-ACP shim, installed under `claude`, `claude-agent-acp`, and `aoe-agent`.
- `stop()` does `SIGTERM` with a 2s wait, `SIGKILL` fallback, then `rm -rf home`. Never calls `tmux kill-server` (would kill the developer's tmux).
- `restart()` kills the running proc (`SIGTERM` then `SIGKILL` after 2s) and respawns with the same args on the same port. Used by connectivity-recovery specs (`disconnect-banner.spec.ts`) that need to observe `setServerDown(true)` on SIGTERM and `setServerDown(false)` after the server comes back. Token mode re-reads the freshly written `serve.token` so `handle.authToken` tracks the second boot. Does NOT re-run passphrase prelogin or structured view master-enable across the restart; specs that need those should call `spawnAoeServe` again.

Binary resolution: `AOE_E2E_BINARY` env wins; otherwise `<repo>/target/release/aoe`. `liveGlobalSetup.ts` runs once before any worker and calls `cargo build --features serve --release` if the binary is missing.

## Fake ACP agent (`web/tests/helpers/fakeAcpAgent.mjs`)

Structured view specs need a deterministic ACP agent because the real `claude` subprocess depends on Anthropic credentials and emits non-deterministic output. The fake speaks the minimal slice of the Agent Client Protocol over newline-delimited JSON-RPC 2.0:

- `initialize` returns protocolVersion 1 + agentCapabilities.
- `session/new` and `session/load` return a deterministic sessionId.
- `session/prompt` consumes one entry from a script file (path supplied via `FAKE_ACP_SCRIPT` env), emits its scripted `session/update` notifications, then responds with `stopReason`. Default script (used when env is absent) emits one `agent_message_chunk` then stops.
- `session/setMode` responds and emits `current_mode_changed`.
- `session/cancel` responds and emits `stopped { stopReason: "cancelled" }`.
- Other methods return `-32601 Method not found`.

Script file shape:

```json
{
  "turns": [
    {
      "updates": [
        { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "..." } },
        { "sessionUpdate": "permission_request", "nonce": "fake-nonce", "toolCall": { "id": "...", "title": "...", "kind": "edit" } }
      ],
      "stopReason": "end_turn"
    }
  ]
}
```

Specs that need a custom script call `spawnAoeServe({ acp: true, fakeAcpScript: "/tmp/script.json", ... })` directly instead of using the `serveAcp` fixture.

## Structured view user-story specs

`web/tests/live/acp-stories/` holds UI-driven structured view specs that
drive the React surface end-to-end (clicks, keystrokes, navigation) and
assert on rendered DOM. They complement the REST-contract specs at
`web/tests/live/acp-*.spec.ts`, which assert against
`/api/sessions/:id/acp/replay`. The story specs catch reducer-to-render
plumbing breakage that the REST tracers cannot see.

Pattern:

```ts
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../../helpers/acp";

base("send message via Enter renders agent chunk", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story");
    if (!seeded) throw new Error("seeded session 'story' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    await page.goto(`${serve.baseUrl}/session/${sessionId}`);
    await waitForStructuredView(page);
    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("hello");
    await composer.press("Enter");
    await expect(page.getByText(/Hello from fake ACP agent/)).toBeVisible();
  } finally {
    await serve.stop();
  }
});
```

`enableStructuredViewAndWait` posts to `/acp/enable`, asserts the response
was 2xx (so a 4xx/5xx surfaces immediately rather than as a noisy
readiness timeout), and then waits for the supervisor handshake.
`waitForStructuredView` waits for the React tree to mount the composer.
Together they ensure both sides are ready before any click or keystroke.
Look up the seeded session by `title` rather than `sessions[0]` so the
spec stays deterministic if seeding adds more rows later.

Custom per-spec scripts go through a temp file (see
`acp-stories/approval-allow.spec.ts` or `acp-approval.spec.ts`
for the canonical setup); the `serveAcp` fixture is for stories
happy with the default chunk-then-stop script.

## Coverage matrix

`web/tests/coverage-matrix.json` is the source of truth for "what does each spec cover". Every entry has:

```json
{
  "id": "auth.passphrase-login",
  "kind": "live-playwright",          // live-playwright | mocked-playwright | vitest | deferred | out-of-scope
  "risk": "high",                      // high | medium | low | n/a
  "specs": ["tests/live/auth-login-passphrase.spec.ts"],
  "components": ["web/src/components/LoginPage.tsx"]
}
```

`deferred` entries also have `issue: "<URL>"`; `out-of-scope` entries have `reason: "<string>"`.

`web/tests/coverage-matrix.exempt.json` lists component files intentionally not assigned to a surface (small presentational primitives covered transitively).

`web/tests/validate-coverage-matrix.mjs` runs in CI on every PR. It fails if:

- A referenced spec file is missing.
- A `deferred` entry has no `issue` URL.
- An `out-of-scope` entry has no `reason`.
- A `.tsx` file under `web/src/components/**` appears in neither the matrix nor the exempt list.
- An exempt entry has no `reason`.

Add a new surface to the matrix at the same time you add the spec. Add a new component to either the matrix or the exempt list at the same time you create the file. CI catches you on the same PR otherwise.

## Coverage reports

Vitest writes `coverage/vitest/` via `@vitest/coverage-v8` (set in `web/vite.config.ts`'s `test.coverage` block).

Playwright collects `window.__coverage__` after each test when `AOE_COVERAGE=1` is set. The instrumentation is added by `vite-plugin-istanbul`, conditionally registered in `web/vite.config.ts`. `build.rs` honors the same env so the embedded web bundle in the `aoe` binary carries instrumentation when requested.

`npm run coverage:merge` runs `web/scripts/merge-coverage.mjs`, which feeds both inputs into `monocart-coverage-reports` and emits:

- `web/coverage/merged/lcov.info`
- `web/coverage/merged/coverage-summary.json` and `coverage-final.json`
- `web/coverage/merged/index.html`

The CI `coverage` job:

1. Builds aoe with `AOE_COVERAGE=1 cargo build --features serve --release`.
2. Runs Vitest with `--coverage`.
3. Runs mocked + live Playwright with `AOE_COVERAGE=1`.
4. Merges via the merge script.
5. Posts a PR comment via `davelosert/vitest-coverage-report-action` with per-file deltas against the latest main-branch baseline artifact.
6. Optionally uploads to codecov.io if `CODECOV_TOKEN` is set.

Report-only in this PR. Phase-2 threshold floor and phase-3 ratchet upward are tracked in issue #1225.

## Gotchas

- `--no-auth`, `--passphrase`, and `--auth=token` are the supported auth modes. Token-mode specs need a debug-build `aoe` because `AOE_TEST_TOKEN_LIFETIME_SECS` and `AOE_TEST_TOKEN_GRACE_SECS` are gated behind `cfg!(debug_assertions)`; release builds keep the production 24h/4h lifetimes and 300s grace.
- The fake-ACP agent does not delegate FS or terminal calls back to the supervisor. If a scripted turn emits `tool_call_started` for a tool that would normally call `fs/write`, the supervisor will not actually write anything (and the test should not assume it does).
- Structured view replay uses the in-memory broadcast channel plus the SQLite event store. If a test asserts a specific event in the replay, give the supervisor up to a few hundred ms to flush (the included tracer specs poll for up to 6 seconds).
- Synthetic touchmove events for mobile specs fire back-to-back with Δt≈1ms. Cap velocity and per-frame emit counts in production code, or a real device will look sane while the e2e produces runaway momentum (and vice versa).

## Adding a new live spec

1. Create `web/tests/live/<surface>.spec.ts`.
2. Import from `../helpers/liveTest`.
3. Pick a fixture (`serve`, `servePassphrase`, `serveToken`, `serveReadOnly`, `serveAcp`) or call `spawnAoeServe()` directly if you need custom options.
4. Add (or update) the matching surface entry in `web/tests/coverage-matrix.json`. Make sure every component the spec touches is in its `components[]` (or already in another surface or the exempt list).
5. Run `node web/tests/validate-coverage-matrix.mjs` locally. CI runs the same script.
6. Run `npx playwright test --config=playwright.live.config.ts <your spec>` to confirm it passes. Live specs require tmux installed and `cargo build --features serve --release` to have run at least once.

## Adding a Vitest contract test

1. Create `web/src/<area>/__tests__/<Thing>.test.tsx` (or co-locate as `<Thing>.test.tsx`).
2. Use `// @vitest-environment jsdom` at the top if the test renders React.
3. Import the component, mount it with React Testing Library, fire events, assert on callback invocations (and `MSW` interceptions if the component makes real `fetch` calls).
4. Update the coverage matrix entry's `kind` to `vitest` and the `specs` path.
5. Run `npm run test:unit -- <your test>`.
