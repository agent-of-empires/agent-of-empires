// User story: recall and edit queued follow-ups with ArrowUp/ArrowDown.
//
// While a turn is active, follow-ups stash onto the QueuedPromptsStrip.
// With the composer empty, ArrowUp walks backward through the queue
// (newest first), loading each prompt into the composer for editing;
// ArrowDown walks forward and past the newest restores the draft.
// Submitting while browsing edits that queued entry in place rather than
// enqueuing a duplicate. See #2147.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait, attachServeDiagnostics } from "../../helpers/acp";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Working on turn 1..." },
        },
        { sessionUpdate: "wait_ms", ms: 30_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("recall queued prompts with ArrowUp and edit in place", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-queue-recall-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-queue-recall" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-queue-recall");
    if (!seeded) throw new Error("seeded session 'story-queue-recall' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });

    // Kick off the long-running turn so follow-ups queue.
    await composer.fill("kick off");
    await composer.press("Enter");
    await expect(page.getByText("Working on turn 1...")).toBeVisible({ timeout: 10_000 });

    // Queue two follow-ups.
    const queueButton = page.getByRole("button", { name: /Queue follow-up message/i });
    await composer.fill("first queued");
    await queueButton.click();
    await composer.fill("second queued");
    await queueButton.click();
    await expect(page.getByRole("button", { name: /^second queued$/ })).toBeVisible({ timeout: 5_000 });

    // Composer empty: ArrowUp enters recall, loading the newest prompt and
    // surfacing the editing banner.
    await expect(composer).toHaveValue("");
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await expect(page.getByText(/Editing queued message 1 of 2/)).toBeVisible();

    // ArrowUp again walks to the older entry.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("first queued");
    await expect(page.getByText(/Editing queued message 2 of 2/)).toBeVisible();

    // ArrowDown walks back toward newer.
    await composer.press("ArrowDown");
    await expect(composer).toHaveValue("second queued");

    // Esc abandons the browse and restores the stashed draft (empty here),
    // clearing the banner.
    await composer.press("Escape");
    await expect(composer).toHaveValue("");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);

    // Re-enter and edit: submitting while browsing updates the queued entry
    // in place, no duplicate, and the editing banner clears.
    await composer.press("ArrowUp");
    await expect(composer).toHaveValue("second queued");
    await expect(page.getByText(/Editing queued message 1 of 2/)).toBeVisible();
    await composer.fill("second queued edited");
    await composer.press("Enter");
    await expect(page.getByText(/Editing queued message/)).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^second queued edited$/ })).toBeVisible({ timeout: 5_000 });
    await expect(page.getByRole("button", { name: /^second queued$/ })).toHaveCount(0);
    // The other entry is untouched and the queue still holds exactly two.
    await expect(page.getByRole("button", { name: /^first queued$/ })).toBeVisible();
    await expect(composer).toHaveValue("");
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
