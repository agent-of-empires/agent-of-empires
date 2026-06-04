// structured_view_seen telemetry activation (#1882).
//
// The backend path for `structured_view_seen` shipped with #1863 (endpoint,
// AtomicBool, snapshot swap-on-read) and the `reportTelemetrySeen` helper
// accepts `surface: "structured_view"`, but no frontend caller ever passed
// `"structured_view"`, so the flag was always false. These specs pin the activated
// behavior: opening a structured view session fires the structured view seen-ping, and a
// read-only server short-circuits the shared guard so no ping is sent.

import { test, expect } from "../helpers/liveTest";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../helpers/acp";

/** Capture every `POST /api/telemetry/seen` body the browser sends, parsed
 *  into `{ surface }`. Attach before `page.goto` so the on-load `"web"`
 *  ping and the structured view ping are both observed. */
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

test("opening a structured view session fires the structured view seen-ping", async ({
  page,
}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-seen-ping" }),
  });
  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId: string = sessions[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    const pings = captureSeenPings(page);
    await page.goto(`${serve.baseUrl}/session/${sessionId}`);
    await waitForStructuredView(page);

    // The structured view mount fires `reportTelemetrySeen("structured_view")`. Pre-fix no
    // caller passed `"structured_view"`, so this poll timed out (the bug).
    await expect
      .poll(() => pings.some((p) => p.surface === "structured_view"), {
        timeout: 10_000,
      })
      .toBe(true);

    // The on-load `"web"` ping still fires too; the structured view ping is
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
  // The seen-ping effects (both `"web"` and `"structured_view"`) share the same
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
