// User story: Stop while a tool call is in flight cancels the turn.
//
// ACP's tool_call session/update (status=pending) translates to a
// ToolCallStarted server event so the structured view renders a ToolCard.
// Stop still cancels the turn via runtime.cancelRun().

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
          sessionUpdate: "tool_call",
          toolCallId: "tc-stop-tool-1",
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

base("Stop button cancels a turn during a tool call", async ({ page }, testInfo) => {
  let serveHandle: { home: string } | undefined;
  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-stop-tool-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stop-tool" }),
    });
    serveHandle = serve;

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stop-tool");
    if (!seeded) throw new Error("seeded session 'story-stop-tool' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("run a slow tool");
    await composer.press("Enter");

    const stopButton = page.getByRole("button", { name: "Stop" });
    await expect(stopButton).toBeVisible({ timeout: 10_000 });
    await stopButton.click();

    await expect(page.getByRole("textbox", { name: /Send a message/i })).toBeVisible({ timeout: 10_000 });
    await expect(stopButton).toBeHidden({ timeout: 10_000 });
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
