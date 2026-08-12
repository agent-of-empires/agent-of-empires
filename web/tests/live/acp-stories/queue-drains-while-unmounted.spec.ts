// User story: queue a follow-up on a structured view session, then leave
// that chat entirely. The queued prompt should reach the agent without
// the user ever navigating back to it. See #3331.
//
// The sibling spec `queue-follow-up-with-nav.spec.ts` navigates away and
// BACK before asserting, so it proves remount recovery rather than
// background delivery. This one never returns to the session: the
// assertion is the server's own replay log, polled while the browser sits
// on `/settings`.
//
// Navigating with `page.goto` is a full reload, which destroys the SPA's
// module state. That is deliberate: it is the harder case, and it also
// exercises the restore-from-localStorage arming path (a queue whose
// newest entry is under an hour old rearms for background delivery).
//
// One structured view session only, for the same reason the sibling spec
// gives: a second supervisor worker is enough to make the first idle out
// on a 4-worker CI runner before turn 2 can fire.

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

const QUEUED_TEXT = "drained-while-away";
const TURN_TWO_TEXT = "Second turn while away.";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "First turn." },
        },
        // Long enough to queue and leave the chat while turn 1 runs, but
        // under the supervisor's resume idle grace (see
        // `RESUME_IDLE_GRACE_DEFAULT` in src/acp/acp_client.rs).
        { sessionUpdate: "wait_ms", ms: 6_000 },
      ],
      stopReason: "end_turn",
    },
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: TURN_TWO_TEXT },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("a queued follow-up drains while its chat is closed", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-queue-unmounted-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "queue-unmounted-a" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const session = sessions.find((s) => s.title === "queue-unmounted-a");
    if (!session) throw new Error("seeded session 'queue-unmounted-a' missing");

    await enableStructuredViewAndWait(serve.baseUrl, session.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(session.id)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off");
    await composer.press("Enter");
    await expect(page.getByText("First turn.")).toBeVisible({ timeout: 10_000 });

    // The queue button only renders while the turn is active, and a stale
    // React batch can leave the send button up for a few ms after the
    // first chunk paints.
    const queueBtn = page.getByRole("button", {
      name: /Queue follow-up message/i,
    });
    await expect(queueBtn).toBeVisible({ timeout: 5_000 });
    await composer.fill(QUEUED_TEXT);
    await queueBtn.click();

    // Parked, and the strip says so. Waiting on the strip also guarantees
    // the queue reached localStorage before the reload below.
    await expect(page.getByText(/Queued \(1\)/i)).toBeVisible({ timeout: 5_000 });

    // Leave the chat. Settings has no structured view and no PTY, so the
    // session's runtime is gone from the page.
    await page.goto(`${serve.baseUrl}/settings`);
    await expect(page).toHaveURL(/\/settings/, { timeout: 10_000 });
    await expect(page.locator("select").first()).toBeVisible({ timeout: 10_000 });

    // Delivered without ever reopening the session: the queued text
    // reaches the agent and turn 2 answers it. Asserted against the
    // server's replay log, not the DOM, precisely because the session's
    // chat is not on screen.
    await waitForReplayContains(serve.baseUrl, session.id, [QUEUED_TEXT, TURN_TWO_TEXT], {
      mode: "all",
      timeoutMs: 25_000,
    });

    // Still on settings; nothing navigated us back.
    await expect(page).toHaveURL(/\/settings/);

    // Exactly once, counted on the user's side of the transcript rather
    // than the agent's. The fake agent falls back to a generic turn once
    // its scripted turns run out (see `fakeAcpAgent.mjs`), so a duplicate
    // send would NOT produce a second `TURN_TWO_TEXT` and counting agent
    // chunks could never fail. Counting `UserPromptSent` frames can.
    const replay = await fetch(`${serve.baseUrl}/api/sessions/${session.id}/acp/replay?since=0`).then((r) => r.json());
    const frames: { event?: { UserPromptSent?: { text?: string } } }[] = Array.isArray(replay)
      ? replay
      : (replay?.frames ?? []);
    const sends = frames.filter((f) => f.event?.UserPromptSent?.text === QUEUED_TEXT);
    expect(sends).toHaveLength(1);
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
