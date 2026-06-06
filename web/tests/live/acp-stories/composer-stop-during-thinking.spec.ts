// User story: Stop while the agent is "thinking" (emitting
// agent_thought_chunk updates) cancels the turn.
//
// ACP's agent_thought_chunk session/update translates to
// ThinkingStarted on the server, which the structured view reducer surfaces
// via the thinking indicator. The Stop affordance is still the
// generic cancelRun() and ends the turn.

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
} from "../../helpers/acp";

const SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_thought_chunk",
          content: { type: "text", text: "Reasoning about the problem..." },
        },
        { sessionUpdate: "wait_ms", ms: 30_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("Stop button cancels a thinking turn", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-stop-think-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(SCRIPT));

  let serve: Awaited<ReturnType<typeof spawnAoeServe>> | undefined;

  try {
    serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      fakeAcpScript: scriptPath,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-stop-thinking" }),
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-stop-thinking");
    if (!seeded)
      throw new Error("seeded session 'story-stop-thinking' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
    );
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("think about this");
    await composer.press("Enter");

    const stopButton = page.getByRole("button", { name: "Stop" });
    await expect(stopButton).toBeVisible({ timeout: 10_000 });
    await stopButton.click();

    await expect(
      page.getByRole("textbox", { name: /Send a message/i }),
    ).toBeVisible({ timeout: 10_000 });
    await expect(stopButton).toBeHidden({ timeout: 10_000 });
  } finally {
    try {
      if (serve) await serve.stop();
    } finally {
      rmSync(scriptDir, { recursive: true, force: true });
    }
  }
});
