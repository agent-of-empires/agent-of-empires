// Live-backend Playwright config.
//
// Live specs under `tests/live/` spawn a real `aoe serve` per test via
// `tests/helpers/aoeServe.ts`, with isolated HOME/XDG/TMPDIR/TMUX_TMPDIR
// and a worker-indexed port range. There is no global Vite preview server;
// each test gets its own backend.
//
// Use `npx playwright test --config=playwright.live.config.ts`.
//
// CI runs this from the `playwright-live` job (see .github/workflows/ci.yml)
// after building `aoe` with `--features serve --release`. Local runs pick up
// the binary from `AOE_E2E_BINARY` or fall back to `../target/release/aoe`.

import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/live",
  globalSetup: "./tests/helpers/liveGlobalSetup.ts",
  // Live specs do real I/O (cargo binary spawn, tmux, fetch). Give them more
  // headroom than the mocked suite's 30s.
  timeout: 60_000,
  // Three workers on the 4-core GitHub runner. Each worker runs a real
  // chromium, a debug-build `aoe serve`, a tmux instance, and (for
  // structured view specs) a fake-ACP node subprocess; with v8 coverage
  // instrumented, 4 workers reliably starved one of them just long
  // enough for whichever spec was waiting on a tight turnActive /
  // WebSocket window to time out. The failing spec rotated across
  // runs (queue follow-up, stop-during-sub-agent, profile-switch-view,
  // etc.) because the contention bit a different worker each time, not
  // because the spec itself was broken: all of those passed locally
  // under the CI-matching debug+AOE_COVERAGE config at 4 workers in
  // isolation but flaked when the full suite ran on the GH Actions
  // runner. 6 → 4 also came from a CI-flake shrink (#1383); this is
  // the next step on the same axis. Each worker still gets an
  // isolated HOME / TMUX_TMPDIR and (workerIndex,
  // parallelIndex)-derived port, so workers don't collide on port or
  // filesystem.
  fullyParallel: true,
  workers: 3,
  // No retries: a flaky live spec doubles CI wall time on every flake.
  // Better to surface the flake and fix it (or quarantine via test.skip
  // until fixed) than to mask it with a retry budget.
  retries: 0,
  use: {
    headless: true,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },
  reporter: process.env.CI
    ? [
        ["html", { open: "never", outputFolder: "playwright-live-report" }],
        ["github"],
        ["junit", { outputFile: "test-report.junit.xml" }],
      ]
    : "list",
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
