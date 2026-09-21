import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/capture",
  timeout: 120_000,
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: "list",
  use: {
    headless: true,
    reducedMotion: "reduce",
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
});
