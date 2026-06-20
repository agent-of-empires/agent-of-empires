// Playwright globalSetup for the live config.
//
// Runs exactly once before any worker spawns. Ensures the `aoe` binary
// exists so per-test `spawnAoeServe()` calls don't pay cargo-build startup
// cost or race each other on a cold build cache.
//
// Behavior:
// - If `AOE_E2E_BINARY` is set and the file exists, do nothing.
// - Else if `<repo>/target/release/aoe` exists, do nothing.
// - Else run `cargo build --release` from the repo root (default features include serve).
//
// CI sets `AOE_E2E_BINARY` (see `.github/workflows/ci.yml`) so the build
// happens in a dedicated job step where the output is visible. Local dev
// gets the convenience of an automatic build on first run.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const repoRoot = resolve(__dirname, "..", "..", "..");

function defaultBinary(): string {
  return join(repoRoot, "target", "release", "aoe");
}

export default async function globalSetup(): Promise<void> {
  const fromEnv = process.env.AOE_E2E_BINARY;
  if (fromEnv && existsSync(fromEnv)) {
    process.stdout.write(`[liveGlobalSetup] using AOE_E2E_BINARY=${fromEnv}\n`);
    return;
  }

  const fallback = defaultBinary();
  if (existsSync(fallback)) {
    process.stdout.write(`[liveGlobalSetup] using ${fallback}\n`);
    return;
  }

  process.stdout.write(`[liveGlobalSetup] building aoe via 'cargo build --release'...\n`);
  const result = spawnSync("cargo", ["build", "--release"], {
    cwd: repoRoot,
    stdio: "inherit",
  });
  if (result.status !== 0) {
    throw new Error(`cargo build --release failed with status ${result.status}`);
  }
  if (!existsSync(fallback)) {
    throw new Error(`cargo build succeeded but ${fallback} is missing`);
  }
  process.stdout.write(`[liveGlobalSetup] built ${fallback}\n`);
}
