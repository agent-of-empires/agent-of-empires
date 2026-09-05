// User story: an agent that puts a question in the permission option
// list (pi's `ask_user_question` sends one `allow_once` option per
// answer) gets its own labels rendered, and clicking one sends back
// that option instead of the first. See #3741.
//
// The fake echoes the option it received as `permission_option=<id>`,
// so the transcript proves which answer reached the agent.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

const OPTION_NAMES = ["Option Alpha", "Option Bravo", "Option Charlie", "Option Delta"];

const QUESTION_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "permission_request",
          toolCall: {
            toolCallId: "pi-ui-1",
            title: "Pi select",
            kind: "other",
            rawInput: { message: "Which option?" },
          },
          options: OPTION_NAMES.map((name, index) => ({
            optionId: `choice-${index}`,
            name,
            kind: "allow_once",
          })),
          echoDecision: true,
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

base("an option-list permission request renders its own labels", async ({ page }, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-story-question-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(QUESTION_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-question" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-question");
    if (!seeded) throw new Error("seeded session 'story-question' missing");
    const sessionId = seeded.id;

    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.fill("ask me something");
    await composer.press("Enter");

    const questionDialog = page.getByRole("alertdialog", { name: /Question/i });
    await expect(questionDialog).toBeVisible({ timeout: 10_000 });

    // The whole point: the agent's labels, not Allow / Always / Deny.
    for (const name of OPTION_NAMES) {
      await expect(questionDialog.getByRole("button", { name })).toBeVisible();
    }
    await expect(questionDialog.getByRole("button", { name: "Allow" })).toHaveCount(0);
    await expect(questionDialog.getByRole("button", { name: "Always" })).toHaveCount(0);

    await questionDialog.getByRole("button", { name: "Option Charlie" }).click();

    // The third option, not the first: the bug this story pins is that
    // Allow silently answered with options[0].
    await expect(page.getByText("permission_option=choice-2")).toBeVisible({ timeout: 10_000 });
    await expect(questionDialog).toBeHidden({ timeout: 10_000 });
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
