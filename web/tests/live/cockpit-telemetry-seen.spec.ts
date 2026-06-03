// cockpit_seen telemetry activation (#1882).
//
// The backend path for `cockpit_seen` shipped with #1863 (endpoint,
// AtomicBool, snapshot swap-on-read) and the `reportTelemetrySeen` helper
// accepts `surface: "cockpit"`, but no frontend caller ever passed
// `"cockpit"`, so the flag was always false. These specs pin the activated
// behavior: opening a cockpit session fires the cockpit seen-ping, and a
// read-only server short-circuits the shared guard so no ping is sent.

import { test, expect } from "../helpers/liveTest";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableCockpitAndWait, waitForCockpitView } from "../helpers/cockpit";

/** Capture every `POST /api/telemetry/seen` body the browser sends, parsed
 *  into `{ surface }`. Attach before `page.goto` so the on-load `"web"`
 *  ping and the cockpit ping are both observed. */
function captureSeenPings(
  page: import("@playwright/test").Page,
): Array<{ surface?: string }> {
  const pings: Array<{ surface?: string }> = [];
  page.on("request", (req) => {
    if (
      req.method() === "POST" &&
      req.url().includes("/api/telemetry/seen")
    ) {
      const body = req.postData();
      if (!body) return;
      try {
        pings.push(JSON.parse(body));
      } catch {
        // Ignore unparseable bodies; the assertions only care about the
        // well-formed `{ surface }` posts the helper emits.
      }
    }
  });
  return pings;
}

test("opening a cockpit session fires the cockpit seen-ping", async ({
  page,
}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    cockpit: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "cockpit-seen-ping" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableCockpitAndWait(serve.baseUrl, sessionId);

    const pings = captureSeenPings(page);
    await page.goto(`${serve.baseUrl}/session/${sessionId}`);
    await waitForCockpitView(page);

    // The cockpit mount fires `reportTelemetrySeen("cockpit")`. Pre-fix no
    // caller passed `"cockpit"`, so this poll timed out (the bug).
    await expect
      .poll(() => pings.some((p) => p.surface === "cockpit"), {
        timeout: 10_000,
      })
      .toBe(true);

    // The on-load `"web"` ping still fires too; the cockpit ping is
    // additive, not a replacement.
    expect(pings.some((p) => p.surface === "web")).toBe(true);
  } finally {
    await serve.stop();
  }
});

test("a read-only server sends no telemetry seen-ping", async ({
  serveReadOnly,
  page,
}) => {
  // The seen-ping effects (both `"web"` and `"cockpit"`) share the same
  // guard: skip on read-only servers, which can't persist a snapshot. The
  // backend also rejects `POST /api/telemetry/seen` with 403 in read-only,
  // but the frontend should never get that far.
  const pings = captureSeenPings(page);

  const aboutPromise = page.waitForResponse(
    (r) => r.url().endsWith("/api/about") && r.status() === 200,
    { timeout: 10_000 },
  );
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  // Settle so React commits the read-only serverAbout state and any effect
  // that was going to fire would have fired.
  await page.waitForTimeout(500);

  expect(pings).toHaveLength(0);
});
