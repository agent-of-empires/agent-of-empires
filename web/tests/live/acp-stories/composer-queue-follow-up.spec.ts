// User story: queue a follow-up message during an active turn.
//
// Script has two turns. Turn 1 emits a chunk, then waits, then ends.
// While turn 1 is alive the composer placeholder flips to "Queue a
// follow-up…" and the QueueSendButton replaces the Send button. The
// user types a second message and clicks Queue; the structured view hook
// stashes it on `queuedPrompts`. When turn 1 ends, the drain effect
// dispatches it as the next prompt and turn 2 emits its distinct text.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import {
  waitForStructuredView,
  enableStructuredViewAndWait,
  attachServeDiagnostics,
} from "../../helpers/acp";

const QUEUE_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "First turn response." },
        },
        // Long enough that the queued follow-up always lands before
        // turn 1 ends on slow CI runners, but well under any 10s
        // idle/watchdog window in the structured view supervisor (see
        // `RESUME_IDLE_GRACE_DEFAULT` in src/acp/acp_client.rs).
        // Earlier rounds bounced between 600ms (raced on slow CI) and
        // 10s (hit the idle watchdog and the worker was torn down
        // before turn 2 could fire). 4s gives the spec ~3.5s of slack
        // to fill+click after observing the first chunk while keeping
        // the total turn well clear of any supervisor watchdog.
        { sessionUpdate: "wait_ms", ms: 4_000 },
      ],
      stopReason: "end_turn",
    },
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Second turn response." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("queued follow-up fires when first turn ends", async ({ page }, testInfo) => {
  // Hoisted so the `finally` block can attach diagnostics even if
  // spawnAoeServe itself throws.
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-queue-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(QUEUE_SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-queue" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-queue");
    if (!seeded) throw new Error("seeded session 'story-queue' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off the first turn");
    await composer.press("Enter");

    // Wait for the first chunk so we know the turn is live.
    await expect(page.getByText("First turn response.")).toBeVisible({
      timeout: 10_000,
    });

    // Wait for the QueueSendButton to actually be present before
    // typing / clicking. The composer only renders it while
    // `turnActive=true`; if the worker died or the reducer is still
    // batching state updates, the SendButton (different aria-label,
    // "Send message" or "Queue message until session resumes") is
    // there instead and clicking by Queue's aria-label would block on
    // a button that never appears.
    const queueBtn = page.getByRole("button", { name: /Queue follow-up message/i });
    await expect(queueBtn).toBeVisible({ timeout: 5_000 });
    await composer.fill("second please");
    await queueBtn.click();

    // Turn 1 wait_ms elapses → end_turn → drain effect fires the
    // queued prompt → turn 2 starts and emits its distinct text.
    await expect(page.getByText("Second turn response.")).toBeVisible({
      timeout: 15_000,
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
