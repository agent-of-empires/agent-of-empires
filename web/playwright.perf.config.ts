import { defineConfig } from "@playwright/test";

// Perf harness: not picked up by playwright.config.ts (it only matches *.spec.ts).
export default defineConfig({
  testDir: "./tests/perf",
  testMatch: "**/*.perf.ts",
  globalSetup: "./tests/helpers/liveGlobalSetup.ts",
  timeout: 300_000,
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: "list",
  use: {
    headless: true,
    viewport: { width: 1440, height: 900 },
  },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
});
