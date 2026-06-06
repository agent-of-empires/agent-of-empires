// User story: edit a queued follow-up message inline.
//
// While a turn is active, the composer's Queue button stashes the
// follow-up onto the QueuedPromptsStrip. Clicking the rendered row
// flips it into a textarea (QueuedPromptEditor); Enter saves the edit
// and the row re-renders with the new text.

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

base("edit a queued follow-up before it fires", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-queue-edit-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-queue-edit" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-queue-edit");
    if (!seeded) throw new Error("seeded session 'story-queue-edit' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
    );
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await composer.fill("kick off");
    await composer.press("Enter");

    await expect(page.getByText("Working on turn 1...")).toBeVisible({
      timeout: 10_000,
    });
    await composer.fill("original queued text");
    await page
      .getByRole("button", { name: /Queue follow-up message/i })
      .click();

    const queuedRow = page.getByRole("button", {
      name: /^original queued text$/,
    });
    await expect(queuedRow).toBeVisible({ timeout: 5_000 });

    await queuedRow.click();
    // The QueuedPromptEditor autofocuses its textarea on mount; the
    // composer textarea was blurred by the row click, so :focus picks
    // out the editor unambiguously.
    const editor = page.locator("textarea:focus");
    await expect(editor).toBeVisible({ timeout: 5_000 });
    await editor.fill("edited queued text");
    await editor.press("Enter");

    await expect(
      page.getByRole("button", { name: /^edited queued text$/ }),
    ).toBeVisible({ timeout: 5_000 });
    await expect(
      page.getByRole("button", { name: /^original queued text$/ }),
    ).toHaveCount(0);
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
