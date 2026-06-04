// User story: drop a queued follow-up message via its X button.
//
// QueuedPromptRow renders a trailing X with title="Drop this queued
// message". Clicking it removes the prompt from the queue strip; the
// row disappears before the active turn ends.

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

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Working on turn 1..." },
        },
        { sessionUpdate: "wait_ms", ms: 8_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("delete a queued follow-up before it fires", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-queue-del-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-queue-del" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-queue-del");
    if (!seeded) throw new Error("seeded session 'story-queue-del' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId, 30_000, serve.home);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    // The composer textarea's accessible name changes with `turnActive`;
    // use a regex that matches both placeholder variants so the same
    // locator works idle and mid-turn.
    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off");
    await composer.press("Enter");

    await expect(page.getByText("Working on turn 1...")).toBeVisible({
      timeout: 10_000,
    });
    await composer.fill("doomed queued text");
    await page.getByRole("button", { name: /Queue follow-up message/i }).click();

    const queuedRow = page.getByRole("button", { name: /^doomed queued text$/ });
    await expect(queuedRow).toBeVisible({ timeout: 5_000 });

    await page.getByTitle("Drop this queued message").click();
    await expect(queuedRow).toHaveCount(0, { timeout: 5_000 });
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
