// Playwright config for documentation screenshot capture.
//
// Runs tests/capture/screenshots.spec.ts, which spawns a seeded
// `aoe serve` (via tests/helpers/aoeServe.ts) and writes hero PNGs into
// docs/assets/. Single worker for determinism; longer timeout because
// cockpit specs drive a scripted ACP turn end to end.
//
// Use scripts/dev/capture-web-screenshots.sh, or:
//   AOE_E2E_BINARY=../target/release/aoe \
//     npx playwright test --config=playwright.capture.config.ts

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
