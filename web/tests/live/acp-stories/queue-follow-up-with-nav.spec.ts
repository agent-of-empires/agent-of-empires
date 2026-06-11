// User story: queue a follow-up on a structured view session, navigate away,
// then return. The queued prompt should still fire once the first
// turn ends.
//
// Single acp-enabled session: kick off a long turn, queue a
// follow-up, navigate away to `/settings` (StructuredView unmounts), then
// back to the session. Replay restores the active turn; once it ends,
// the drained follow-up triggers the second turn.
//
// Earlier iterations of this spec seeded a second structured view session B
// just to use as a navigation target. That left both A's and B's
// supervisor workers running for the full test duration, and on the
// 4-worker CI runner the extra worker pair was enough to make A's
// worker idle out and emit a synthetic `Stopped { reattach_idle }`
// before turn 2 could fire. The same unmount/remount cycle is achieved
// by navigating to `/settings` instead, with one structured view worker in
// flight.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView, attachServeDiagnostics } from "../../helpers/acp";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "First turn." },
        },
        // Long enough that the navigate-away + navigate-back cycle
        // below completes while turn 1 is still in flight, but well
        // under any 10s idle watchdog in the structured view supervisor (see
        // `RESUME_IDLE_GRACE_DEFAULT` in src/acp/acp_client.rs).
        { sessionUpdate: "wait_ms", ms: 6_000 },
      ],
      stopReason: "end_turn",
    },
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Second turn after nav." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("queued follow-up fires after navigation away and back", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-queue-nav-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "queue-nav-a" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const sessionA = sessions.find((s) => s.title === "queue-nav-a");
    if (!sessionA) throw new Error("seeded session 'queue-nav-a' missing");

    await enableStructuredViewAndWait(serve.baseUrl, sessionA.id, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`);
    await waitForStructuredView(page);

    const composerA = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composerA.fill("kick off A");
    await composerA.press("Enter");
    await expect(page.getByText("First turn.")).toBeVisible({
      timeout: 10_000,
    });

    // Wait for the QueueSendButton to be present before typing /
    // clicking; the composer only renders it while `turnActive=true`,
    // and a stale React batch can leave the SendButton up for a few
    // ms after "First turn." paints.
    const queueBtn = page.getByRole("button", {
      name: /Queue follow-up message/i,
    });
    await expect(queueBtn).toBeVisible({ timeout: 5_000 });
    await composerA.fill("from-after-nav");
    await queueBtn.click();

    // Navigate away to settings (no structured view, no PTY), then back to A.
    // The StructuredView unmount/remount is what we want to exercise; the
    // destination only matters insofar as it is not the same session.
    await page.goto(`${serve.baseUrl}/settings`);
    await expect(page).toHaveURL(/\/settings/, { timeout: 10_000 });
    // The Profile combobox is the most reliable Settings-mounted
    // signal and avoids depending on a particular heading element.
    await expect(page.locator("select").first()).toBeVisible({
      timeout: 10_000,
    });
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionA.id)}`);
    await waitForStructuredView(page);

    // The first turn ends shortly after; the drained follow-up fires
    // turn 2 EXACTLY ONCE and its distinct chunk appears in the
    // transcript. Assert toHaveCount(1) so a regression that double-
    // fires the queued prompt after remount would fail here instead
    // of silently passing on the first occurrence.
    const secondTurn = page.getByText("Second turn after nav.", {
      exact: true,
    });
    await expect(secondTurn).toHaveCount(1, { timeout: 20_000 });
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
