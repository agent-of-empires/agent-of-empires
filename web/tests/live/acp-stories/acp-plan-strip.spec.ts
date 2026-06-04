// User story: agent-emitted plan updates render in the PlanStrip
// above the structured view transcript.
//
// ACP's `plan` session update carries `entries: [{ content, status,
// priority? }]`. The supervisor translates it to PlanUpdated; the
// reducer applies to state.plan and StructuredView mounts PlanStrip,
// which shows the first in-progress step's title.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const PLAN_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "plan",
          entries: [
            { content: "Investigate the bug", status: "in_progress", priority: "high" },
            { content: "Write a fix", status: "pending", priority: "medium" },
            { content: "Add tests", status: "pending", priority: "low" },
          ],
        },
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Planned." },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("plan session update renders in PlanStrip", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-plan-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(PLAN_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-plan" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-plan");
    if (!seeded) throw new Error("seeded session 'story-plan' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("plan this work");
    await composer.press("Enter");

    // PlanStrip shows the current step (first in-progress entry).
    // The same title also appears in the expanded list below, so use
    // `.first()` to scope to the header.
    await expect(
      page.getByText("Investigate the bug").first(),
    ).toBeVisible({ timeout: 15_000 });
    // Progress label "0/3": zero done, three total. Sidebar session
    // row also renders the same "0/3" counter, so `.first()` scopes
    // to the structured view PlanStrip (which mounts before the sidebar one
    // updates).
    await expect(page.getByText("0/3").first()).toBeVisible({ timeout: 15_000 });
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
