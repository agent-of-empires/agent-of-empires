// Playwright globalSetup for the live config.
//
// Runs exactly once before any worker spawns. Ensures the debug `aoe` binary
// exists so per-test `spawnAoeServe()` calls don't pay cargo-build startup
// cost or race each other on a cold build cache. Live tests use debug-only
// timing overrides, matching the binary supplied by CI.
//
// Behavior:
// - If `AOE_E2E_BINARY` is set and the file exists, do nothing.
// - Else if `<repo>/target/debug/aoe` exists, do nothing.
// - Else run `cargo build --features web` from the repo root.
//   `web` is NOT a default feature (a plain build needs no Node/npm). A
//   default build still runs `aoe serve`, but with no dashboard bundle
//   embedded, so every live spec would load a 404 instead of the app.
//
// CI sets `AOE_E2E_BINARY` (see `.github/workflows/tests.yml`) so the build
// happens in a dedicated job step where the output is visible. Local dev
// gets the convenience of an automatic build on first run.

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
