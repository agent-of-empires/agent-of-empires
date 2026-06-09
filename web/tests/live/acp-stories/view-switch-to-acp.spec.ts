// User story: switching view (tmux → structured view) via the
// SwitchViewAction trigger on the terminal view.
//
// Seed a non-structured view session, navigate to it (TerminalView renders),
// click the view-switch icon, confirm in the dialog, and assert
// the parent flips to StructuredView once the session-list poll picks up
// the new `structured_view`.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView } from "../../helpers/acp";

base("view switch from terminal to structured view mounts the structured view", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-view" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const target = sessions.find((s) => s.title === "story-view");
    if (!target) throw new Error("seeded session 'story-view' missing");
    const sessionId = target.id;

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const trigger = page.getByRole("button", {
      name: "Switch to structured view",
    });
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click();

    await expect(page.getByRole("heading", { name: /Switch to structured view/i })).toBeVisible({ timeout: 5_000 });
    await page.getByRole("button", { name: "Switch", exact: true }).click();

    // Session-list poll lands within a few seconds; once structured_view
    // flips, App.tsx renders StructuredView and the composer mounts.
    await waitForStructuredView(page, 20_000);
  } finally {
    await serve.stop();
  }
});
