// Showcase Playwright config: records every mocked AND live spec in one run
// for demo videos, trading throughput for cinematic footage (full-resolution
// video, slow per-action delay, single worker so the encoder owns the CPU).
//
// Usage, build steps, and the ffmpeg post-processing live in
// docs/development/showcase-video.md.

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

  // webServer is config-wide, so it boots for every project: mocked specs
  // hit it on 4173, and a live-only run (--project=live) still starts vite
  // preview here and therefore needs a built bundle, even though live specs
  // spawn their own backend per test.
  webServer: {
    command: "npx vite preview --port 4173",
    port: 4173,
    reuseExistingServer: true,
  },

  reporter: [["html", { open: "never", outputFolder: "playwright-showcase-report" }], ["list"]],

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
