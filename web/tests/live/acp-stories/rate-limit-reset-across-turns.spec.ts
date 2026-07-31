// User story: a session already parked on a rate limit gets another prompt,
// is rejected again, and the banner must still name the real reset time. See
// #3152.
//
// This is the case the previous fix missed. claude-agent-acp forwards the
// reset epoch only on a `usage_update`'s `_meta._claude/rateLimit`, and it
// suppresses that update on any turn that produced no assistant usage, which
// is exactly a turn rejected outright. aoe used to wipe the captured epoch at
// every prompt start, so the second rejection had nothing to report and fell
// back to `now + 1h`.
//
// The script mirrors that: turn 0 carries the rejection epoch, turn 1 gets a
// bare `errorKind: "rate_limit"` with no usage_update at all. The banner after
// turn 1 must show turn 0's distinctive epoch. Before the fix it showed
// `now + 1h`, an hour off and a different clock reading.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView, attachServeDiagnostics } from "../../helpers/acp";

// Deliberately far from `now + 1h` (the fallback this replaces) so a
// regression renders a visibly different time.
const RESET_SECS = Math.floor(Date.now() / 1000) + 3 * 3600 + 41 * 60;
const RESET_ISO = new Date(RESET_SECS * 1000).toISOString();

const SCRIPT = {
  turns: [
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Starting the task." } },
        {
          sessionUpdate: "usage_update",
          used: 4321,
          size: 200000,
          _meta: { "_claude/rateLimit": { status: "rejected", rateLimitType: "five_hour", resetsAt: RESET_SECS } },
        },
      ],
      rateLimit: { message: "usage limit reached" },
    },
    {
      // The retry is rejected before producing anything, so the adapter sends
      // no usage_update and the error carries no reset. The epoch captured on
      // turn 0 is the only source left.
      updates: [],
      rateLimit: { message: "usage limit reached" },
    },
  ],
};

/** Reset epochs (unix seconds) of every `RateLimit` event in the replay, in
 *  order. `null` for an event whose reset is unknown. */
async function rateLimitResets(baseUrl: string, sessionId: string): Promise<(number | null)[]> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  const frames: { event?: { RateLimit?: { info?: { resets_at?: string | null } } } }[] = Array.isArray(replay)
    ? replay
    : (replay?.frames ?? []);
  return frames
    .filter((f) => f?.event?.RateLimit !== undefined)
    .map((f) => {
      const iso = f.event?.RateLimit?.info?.resets_at;
      return typeof iso === "string" ? Math.floor(new Date(iso).getTime() / 1000) : null;
    });
}

base("a later rejection still reports the reset captured on an earlier turn", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-rl-across-"));
  const scriptPath = join(scriptDir, "script.json");
  const turnStatePath = join(scriptDir, "turn-cursor");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      // Persist the turn cursor so the second prompt gets turn 1, not turn 0.
      extraEnv: { FAKE_ACP_TURN_STATE: turnStatePath },
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "rl-across" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const session = sessions.find((s) => s.title === "rl-across");
    if (!session) throw new Error("seeded session 'rl-across' missing");

    await enableStructuredViewAndWait(serve.baseUrl, session.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(session.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
    await composer.fill("start the task");
    await composer.press("Enter");

    // Rendered the same way the UI does, computed in-browser so locale and
    // timezone match.
    const expectedReset: string = await page.evaluate((iso) => new Date(iso).toLocaleTimeString(), RESET_ISO);
    await expect(page.getByText(`resets at ${expectedReset}`)).toBeVisible({ timeout: 15_000 });

    // Resume re-issues the prompt, which is rejected again. Assert at the
    // source of truth first: the second RateLimit event must carry turn 0's
    // epoch. Before the fix it carried `now + 1h`, so this is the assertion
    // that goes red on the pre-fix tree.
    await page.getByRole("button", { name: /Resume now/i }).click();
    await expect
      .poll(async () => await rateLimitResets(serve!.baseUrl, session.id), {
        timeout: 30_000,
        intervals: [200, 500, 1000],
      })
      .toEqual([RESET_SECS, RESET_SECS]);

    // And the banner the user is looking at still reads that time.
    await expect(page.getByText(`resets at ${expectedReset}`)).toBeVisible({ timeout: 30_000 });
  } finally {
    try {
      if (serveHandle) await attachServeDiagnostics(testInfo, serveHandle);
    } catch {
      // best-effort diagnostics; do not block cleanup
    }
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
