// Showcase Playwright config.
//
// Reuses the existing mocked specs (`tests/*.spec.ts`) AND live-backend
// specs (`tests/live/**/*.spec.ts`) under a single run, trading throughput
// for cinematic, deterministic video recordings. Each test gets a full
// resolution video, a slow per-action delay, and a single worker so the
// encoder never fights another browser for CPU.
//
// Run:
//   cd web
//   npx playwright test --config=playwright.showcase.config.ts
//
// Filter to a single project or spec:
//   npx playwright test --config=playwright.showcase.config.ts --project=mocked
//   npx playwright test --config=playwright.showcase.config.ts tests/live/golden-path.spec.ts
//
// The aoe binary is resolved the same way as the live config
// (AOE_E2E_BINARY env or ../target/release/aoe). Build it first with
// `cargo build --features serve --release` (globalSetup will do that
// for you on first run too).
//
// Videos land under `test-results/**/video.webm`. The HTML report at
// `playwright-showcase-report/` embeds them. Post-process with ffmpeg
// (e.g. `-filter:v "setpts=0.4*PTS" -an`) to speed the final clip up.

import { defineConfig } from "@playwright/test";

const sharedUse = {
  headless: true,
  viewport: { width: 1440, height: 900 },
  video: {
    mode: "on" as const,
    size: { width: 1440, height: 900 },
  },
  trace: "on" as const,
  screenshot: "only-on-failure" as const,
  // Visible pacing. Each Playwright action waits this many ms before
  // executing. ffmpeg speed-up pass compresses the result later.
  launchOptions: {
    slowMo: 250,
  },
};

export default defineConfig({
  // Single worker keeps the recording deterministic and prevents the
  // video encoder from contending with parallel browser instances.
  fullyParallel: false,
  workers: 1,
  retries: 0,

  // slowMo + video encoding stretches wall-clock per test well past the
  // 30s mocked / 60s live defaults.
  timeout: 180_000,

  // Live specs need the aoe binary built; this is the live config's
  // globalSetup and is a no-op when the binary is already on disk.
  globalSetup: "./tests/helpers/liveGlobalSetup.ts",

  // Mocked specs need a vite preview server on 4173. Live specs ignore
  // it (they spawn their own backend per test).
  webServer: {
    command: "npx vite preview --port 4173",
    port: 4173,
    reuseExistingServer: true,
  },

  reporter: [
    ["html", { open: "never", outputFolder: "playwright-showcase-report" }],
    ["list"],
  ],

  projects: [
    {
      name: "mocked",
      testDir: "./tests",
      testIgnore: ["**/live/**"],
      use: {
        ...sharedUse,
        browserName: "chromium",
        baseURL: "http://localhost:4173",
      },
    },
    {
      name: "live",
      testDir: "./tests/live",
      use: {
        ...sharedUse,
        browserName: "chromium",
      },
    },
  ],
});
