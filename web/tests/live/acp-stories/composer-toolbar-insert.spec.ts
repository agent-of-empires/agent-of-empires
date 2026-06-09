// User story: clicking @ or / on the composer toolbar inserts the
// trigger character into the textarea.
//
// ToolbarButton aria-labels are "Add file context (@)" and
// "Slash command (/)"; both call insertAtCaret on the textarea ref.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

base("composer toolbar inserts @ and / into the textarea", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-composer-toolbar" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-composer-toolbar");
    if (!seeded) throw new Error("seeded session 'story-composer-toolbar' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    const composer = page.getByRole("textbox", { name: /Send a message/i });
    await composer.click();

    await page.getByRole("button", { name: "Add file context (@)" }).click();
    // The popover the @ trigger surfaces can insert a trailing space
    // after the trigger character, so use a substring match rather
    // than equality.
    await expect(composer).toHaveValue(/@/);

    await page.getByRole("button", { name: "Slash command (/)" }).click();
    await expect(composer).toHaveValue(/@.*\//);
  } finally {
    await serve.stop();
  }
});
