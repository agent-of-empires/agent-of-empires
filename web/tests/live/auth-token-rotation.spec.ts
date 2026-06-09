// Token rotation grace window: after the daemon rotates, the previous
// token must keep working for the grace period, the new token must work
// immediately, and the previous token must be rejected once grace
// expires. Covers `src/server/mod.rs` TokenManager rotate + validate.
//
// Driven by debug-build env overrides AOE_TEST_TOKEN_LIFETIME_SECS and
// AOE_TEST_TOKEN_GRACE_SECS. The harness sets these via the
// `tokenLifetimeSecs` / `tokenGraceSecs` options on spawnAoeServe. In
// release builds the override is ignored (token lifetime stays at 24h);
// this spec relies on debug-build behavior so the CI matrix that runs
// live specs must build aoe in debug or pass --features serve with
// `cfg!(debug_assertions)` on (current `playwright-live` job does both).

import { readFile } from "node:fs/promises";

import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe } from "../helpers/aoeServe";

const LIFETIME_SECS = 6;
const GRACE_SECS = 3;
const POLL_INTERVAL_MS = 200;

/** Probe a token via Bearer header against an unauthenticated GET. */
async function probeToken(baseUrl: string, token: string): Promise<number> {
  const res = await fetch(`${baseUrl}/api/about`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  // Drain body so the connection releases promptly.
  await res.text().catch(() => "");
  return res.status;
}

/**
 * Poll the daemon's `serve.token` file until its content differs from
 * `previous`. Returns the new token. Times out after `deadlineMs`.
 */
async function waitForRotation(tokenFile: string, previous: string, deadlineMs: number): Promise<string> {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    try {
      const raw = (await readFile(tokenFile, "utf8")).trim();
      if (raw.length > 0 && raw !== previous) return raw;
    } catch {
      // file may be momentarily unlinked during write; retry
    }
    await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
  }
  throw new Error(`token file did not rotate within ${deadlineMs}ms`);
}

test("rotated token: old accepted in grace, new accepted, old rejected past grace", async ({}, testInfo) => {
  test.setTimeout(60_000);

  const handle = await spawnAoeServe({
    authMode: "token",
    tokenLifetimeSecs: LIFETIME_SECS,
    tokenGraceSecs: GRACE_SECS,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    const tokenA = handle.authToken!;
    expect(tokenA).toMatch(/^[0-9a-f]{64}$/);

    // Pre-rotation: tokenA works, garbage does not.
    expect(await probeToken(handle.baseUrl, tokenA)).toBe(200);
    expect(await probeToken(handle.baseUrl, "z".repeat(64))).toBe(401);

    // Wait for the daemon to rotate; lifetime is LIFETIME_SECS, so allow
    // an extra grace+slack to avoid a flake on slow CI.
    const tokenB = await waitForRotation(handle.tokenFile!, tokenA, (LIFETIME_SECS + GRACE_SECS + 5) * 1000);
    expect(tokenB).toMatch(/^[0-9a-f]{64}$/);
    expect(tokenB).not.toBe(tokenA);

    // In-grace window: both tokens valid.
    expect(await probeToken(handle.baseUrl, tokenA)).toBe(200);
    expect(await probeToken(handle.baseUrl, tokenB)).toBe(200);

    // Wait past the grace expiry. Add a small fudge factor because the
    // rotation task's `sleep(grace)` is the bound; the validate-side
    // grace_expires comparison is the actual gate.
    await new Promise((r) => setTimeout(r, (GRACE_SECS + 1) * 1000));

    // After grace: tokenA rejected. tokenB still current. (A second
    // rotation could fire at t = LIFETIME_SECS*2, which is still > the
    // current elapsed time given LIFETIME_SECS=6 and GRACE_SECS=3.)
    expect(await probeToken(handle.baseUrl, tokenA)).toBe(401);
    expect(await probeToken(handle.baseUrl, tokenB)).toBe(200);
  } finally {
    await handle.stop();
  }
});
