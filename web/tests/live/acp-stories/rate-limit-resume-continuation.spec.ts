// User story: a structured view turn is interrupted by a provider rate limit
// and parks the session. When the user clicks "Resume now", aoe must not only
// respawn the worker but also re-issue the interrupted prompt so the agent
// continues instead of sitting idle. See #3028.
//
// Two halves of #3028 get e2e teeth here:
//  1. Reset time: the fake reports the real reset ONLY out-of-band, on a
//     `usage_update`'s `_meta._claude/rateLimit.resetsAt` (the way
//     claude-agent-acp does it), and the rate-limit error carries NO
//     `resets_at`. The banner must render that distinctive time; a regression
//     to the old `now + 1h` guess would show a different time and fail.
//  2. Continuation: the fake rate-limits turn 0, then (via a persisted turn
//     cursor surviving the resume respawn) answers turn 1 with a distinct
//     marker. That marker in the replay proves the interrupted prompt was
//     re-issued on resume.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import {
  enableStructuredViewAndWait,
  waitForStructuredView,
  waitForReplayContains,
  attachServeDiagnostics,
} from "../../helpers/acp";

// A distinctive future reset, deliberately far from `now + 1h` (the fallback
// this PR replaces) so a regression to that guess renders a different time.
const RESET_SECS = Math.floor(Date.now() / 1000) + 2 * 3600 + 37 * 60;
const RESET_ISO = new Date(RESET_SECS * 1000).toISOString();

// Turn 0 rate-limits; turn 1 (served to the resumed worker) is the
// continuation and carries a marker distinct from turn 0. The reset time
// rides ONLY on the usage_update meta, and the error omits resets_at, so the
// banner's time can only be right if the meta-capture path works.
const SCRIPT = {
  turns: [
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Starting the task." } },
        {
          sessionUpdate: "usage_update",
          used: 1234,
          size: 200000,
          _meta: { "_claude/rateLimit": { status: "rejected", resetsAt: RESET_SECS } },
        },
      ],
      rateLimit: { message: "usage limit reached" },
    },
    {
      updates: [
        { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "Resumed and continued the task." } },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("resume re-issues the interrupted prompt so the agent continues", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-rl-resume-"));
  const scriptPath = join(scriptDir, "script.json");
  const turnStatePath = join(scriptDir, "turn-cursor");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      // Persist the fake agent's turn cursor across the resume respawn so the
      // continuation prompt gets turn 1, not turn 0 again.
      extraEnv: { FAKE_ACP_TURN_STATE: turnStatePath },
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "rl-resume" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const session = sessions.find((s) => s.title === "rl-resume");
    if (!session) throw new Error("seeded session 'rl-resume' missing");

    await enableStructuredViewAndWait(serve.baseUrl, session.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(session.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message|Queue a follow-up/i });
    await composer.fill("keep working on the task");
    await composer.press("Enter");

    // The turn parks on the rate limit.
    await expect(page.getByText(/Rate-limited/i)).toBeVisible({ timeout: 15_000 });

    // The banner must show the real reset from the usage_update meta, rendered
    // the same way the UI does (`new Date(resets_at).toLocaleTimeString()`),
    // computed in-browser so locale/timezone match. A regression to the old
    // `now + 1h` guess would render a different time and fail this.
    const expectedReset: string = await page.evaluate((iso) => new Date(iso).toLocaleTimeString(), RESET_ISO);
    await expect(page.getByText(`resets at ${expectedReset}`)).toBeVisible({ timeout: 15_000 });

    // Resume: respawns the worker AND (the #3028 fix) re-issues the
    // interrupted prompt via the pending-initial-turn drain.
    await page.getByRole("button", { name: /Resume now/i }).click();

    // The continuation marker only appears if the interrupted prompt was
    // re-sent to the resumed worker.
    await waitForReplayContains(serve.baseUrl, session.id, "Resumed and continued the task.", {
      timeoutMs: 30_000,
    });
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
