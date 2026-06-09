// User story: open the BasePicker on the diff panel and see the
// branch-suggestions listbox render.
//
// The trigger renders inline next to "Changes" with aria-label "Change
// diff base (current: <branch>)". Clicking it opens a popover with a
// search input and a role=listbox of suggestions.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("BasePicker opens the branch suggestions listbox", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-diff-base" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-diff-base");
    if (!seeded) throw new Error("seeded session 'story-diff-base' missing");
    const sessionId = seeded.id;
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const trigger = page.getByRole("button", {
      name: /Change diff base \(current: /i,
    });
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click();

    await expect(page.getByPlaceholder("Search branches...")).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByRole("listbox", { name: "Branch suggestions" })).toBeVisible();
  } finally {
    await serve.stop();
  }
});
