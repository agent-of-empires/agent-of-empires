import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  // Live-backend specs (spawn real `aoe serve`) live under tests/live/ and
  // run via playwright.live.config.ts. The dev-only screenshot capture spec
  // under tests/capture/ also spawns a real `aoe serve` and runs via
  // playwright.capture.config.ts; keep it out of the mocked suite.
  testIgnore: ["**/live/**", "**/capture/**"],
  timeout: 30000,
  retries: process.env.CI ? 1 : 0,
  // Mocked specs share one `vite preview` server and otherwise touch no
  // shared state, so they're safe to run fully in parallel. Without these
  // settings Playwright defaults to half-CPU workers (2 on the 4-vCPU
  // ubuntu-latest runner) and file-level parallelism only.
  //
  // 6 workers on a 4-vCPU runner: the 4-worker baseline left ~30% of wall
  // time on the table because mocked tests spend most of their wall on IPC
  // roundtrips (page.route, page.evaluate, WebSocket) rather than CPU.
  // Over-subscribing the cores lets a blocked worker yield to another.
  // Local measurements with AOE_COVERAGE=1: 4 workers = 63s, 6 workers = 46s.
  fullyParallel: true,
  workers: process.env.CI ? 6 : undefined,
  use: {
    baseURL: "http://localhost:4173",
    headless: true,
    screenshot: "only-on-failure",
  },
  webServer: {
    command: "npx vite preview --port 4173",
    port: 4173,
    reuseExistingServer: !process.env.CI,
  },
  reporter: process.env.CI
    ? [["html", { open: "never" }], ["github"], ["junit", { outputFile: "test-report.junit.xml" }]]
    : "list",
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
