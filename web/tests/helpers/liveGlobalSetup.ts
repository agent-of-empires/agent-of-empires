// Playwright globalSetup for the live config.
//
// Use the supplied or existing debug web binary; build it if absent. Debug builds
// forward the live harness's explicit watchdog timing control.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const repoRoot = resolve(__dirname, "..", "..", "..");

export default async function globalSetup(): Promise<void> {
  const fromEnv = process.env.AOE_E2E_BINARY;
  if (fromEnv && existsSync(fromEnv)) {
    process.stdout.write(`[liveGlobalSetup] using AOE_E2E_BINARY=${fromEnv}\n`);
    return;
  }

  const fallback = join(repoRoot, "target", "debug", "aoe");
  if (existsSync(fallback)) {
    process.stdout.write(`[liveGlobalSetup] using ${fallback}\n`);
    return;
  }

  process.stdout.write(`[liveGlobalSetup] building aoe via 'cargo build --features web'...\n`);
  const result = spawnSync("cargo", ["build", "--features", "web"], {
    cwd: repoRoot,
    stdio: "inherit",
  });
  if (result.status !== 0) {
    throw new Error(`cargo build --features web failed with status ${result.status}`);
  }
  if (!existsSync(fallback)) {
    throw new Error(`cargo build succeeded but ${fallback} is missing`);
  }
  process.stdout.write(`[liveGlobalSetup] built ${fallback}\n`);
}
