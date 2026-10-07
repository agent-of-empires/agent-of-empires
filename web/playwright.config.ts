import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  testIgnore: ["**/live/**", "**/capture/**"],
  // `.test.ts` files under tests/ are Vitest.
  testMatch: "**/*.spec.ts",
  timeout: 30000,
  retries: process.env.CI ? 1 : 0,
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
