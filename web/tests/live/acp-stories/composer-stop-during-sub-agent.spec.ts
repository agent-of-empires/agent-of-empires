// User story: Stop button cancels a sub-agent task.
//
// ACP child tool calls carry _meta.claudeCode.parentToolUseId so the
// structured view groups them under a parent Task. The Stop affordance still
// uses runtime.cancelRun(); this story exists so a future refactor of
// sub-agent rendering can't break the parent Stop path.

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
          sessionUpdate: "tool_call",
          toolCallId: "parent-task",
          title: "Task: investigate",
          kind: "task",
          status: "pending",
        },
        {
          sessionUpdate: "tool_call",
          toolCallId: "child-read",
          title: "Read file",
          kind: "read",
          status: "pending",
          _meta: { claudeCode: { parentToolUseId: "parent-task" } },
        },
        { sessionUpdate: "wait_ms", ms: 30_000 },
      ],
      stopReason: "end_turn",
    },
  ],
};

base(
  "Stop button cancels a turn during a sub-agent task",
  async ({ page }, testInfo) => {
    const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-stop-sub-"));
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
        seedFn: seedSessionViaAoeAdd({ title: "story-stop-subagent" }),
      });

      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-stop-subagent");
      if (!seeded)
        throw new Error("seeded session 'story-stop-subagent' missing");
      const sessionId = seeded.id;
      await enableStructuredViewAndWait(serve.baseUrl, sessionId);

      await page.goto(
        `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
      );
      await waitForStructuredView(page);

      const composer = page.getByRole("textbox", { name: /Send a message/i });
      await composer.fill("delegate this");
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
  },
);
