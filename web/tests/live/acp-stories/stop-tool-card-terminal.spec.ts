// User story: Stopping mid-tool settles the tool card, it does not get
// stuck "running" with a forever-counting timer. See #1646.
//
// A `tool_call` session/update (status=pending) with no completion
// renders a running ToolCard. Before the fix, the card's status was
// derived solely from a paired completion row, so a cancel (which emits
// only a turn-level Stopped, never a per-tool completion) left the card
// orange "running" forever, live and on reload. The reducer now sweeps
// open tool calls to a terminal "stopped" state on any turn-ending
// event, so the badge settles and the timer freezes.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
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
          sessionUpdate: "tool_call",
          toolCallId: "tc-stuck-1",
          title: "Slow tool",
          kind: "read",
          status: "pending",
        },
        { sessionUpdate: "wait_ms", ms: 30_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

// The tool card whose primary title is "Slow tool". Anchored on the
// exact title span (the prompt echo "run a slow tool" and the spinner
// "Operating Slow tool…" are not exact matches), then walked up to the
// card root so badge text reads only inside this card.
function cardFor(page: import("@playwright/test").Page) {
  return page
    .getByText("Slow tool", { exact: true })
    .locator("xpath=ancestor::div[contains(@class,'rounded-md')][1]");
}

test("stopping mid-tool settles the card and survives reload", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-stuck-tool-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stuck-tool" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stuck-tool");
    if (!seeded) throw new Error("seeded session 'story-stuck-tool' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("run a slow tool");
    await composer.press("Enter");

    // The running tool card mounts; its badge is "running" until the
    // turn ends. Scope assertions to the card so the badge text does not
    // match the prompt echo ("run a slow tool") or the working spinner
    // ("Operating Slow tool…"); `exact` title resolves to the card's
    // primary span alone.
    const card = cardFor(page);
    await expect(card).toBeVisible({ timeout: 10_000 });
    await expect(card.getByText("running", { exact: true })).toBeVisible({
      timeout: 10_000,
    });

    const stopButton = page.getByRole("button", { name: "Stop" });
    await expect(stopButton).toBeVisible({ timeout: 10_000 });
    await stopButton.click();

    // The card settles to the distinct terminal "stopped" state instead
    // of staying "running" forever.
    await expect(card.getByText("stopped", { exact: true })).toBeVisible({
      timeout: 10_000,
    });
    await expect(card.getByText("running", { exact: true })).toBeHidden();

    // The stuck state was persisted before the fix, so a reload must
    // still show the card terminal: the trailing Stopped replays through
    // the same reducer sweep.
    await page.reload();
    await waitForStructuredView(page);
    const cardAfter = cardFor(page);
    await expect(cardAfter).toBeVisible({ timeout: 10_000 });
    await expect(cardAfter.getByText("stopped", { exact: true })).toBeVisible({
      timeout: 10_000,
    });
    await expect(cardAfter.getByText("running", { exact: true })).toBeHidden();
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
