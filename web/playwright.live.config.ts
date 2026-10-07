import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/live",
  globalSetup: "./tests/helpers/liveGlobalSetup.ts",
  timeout: 60_000,
  fullyParallel: true,
  // More workers starve live specs under coverage on the 4-core CI runner.
  workers: 3,
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
