// User story (#1467): a failed tool card auto-opens so the error is
// visible, but the header chevron must still fold it once the user has
// read it.
//
// The fake ACP agent emits a tool_call (pending) to render the card,
// then a tool_call_update with status "failed" carrying the error text.
// `src/acp/acp_client.rs` maps the failed update to a tool_error row,
// so the web `statusFor` resolves to "err" and the card opens on its own.
// Clicking the header collapses the rose error block; clicking again
// re-expands it.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait, attachServeDiagnostics } from "../../helpers/acp";

const ERROR_TEXT = "boom: the command exploded";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "tool_call",
          toolCallId: "tc-fail-1",
          title: "rm -rf /nope",
          kind: "execute",
          status: "pending",
          rawInput: { command: "rm -rf /nope" },
        },
        {
          sessionUpdate: "tool_call_update",
          toolCallId: "tc-fail-1",
          status: "failed",
          content: [
            {
              type: "content",
              content: { type: "text", text: ERROR_TEXT },
            },
          ],
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("failed tool card auto-opens and folds via the chevron", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-fold-fail-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-fold-fail" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-fold-fail");
    if (!seeded) throw new Error("seeded session 'story-fold-fail' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("run the failing command");
    await composer.press("Enter");

    // The failed card auto-opens: both the rose "tool failed" label and
    // the error text are visible without any user interaction.
    const errorText = page.getByText(ERROR_TEXT);
    await expect(errorText).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("tool failed")).toBeVisible();

    // The card header is the only button carrying the "failed" status
    // badge; clicking it folds the body.
    const cardHeader = page
      .getByRole("button")
      .filter({ hasText: /failed/i })
      .first();
    await cardHeader.click();
    await expect(errorText).toBeHidden({ timeout: 10_000 });

    // Clicking again re-expands it.
    await cardHeader.click();
    await expect(errorText).toBeVisible({ timeout: 10_000 });
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
