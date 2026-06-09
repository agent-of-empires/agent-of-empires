// User story: fold and unfold a sidebar session group.
//
// Each group renders a sidebar-group-header with an aria-expanded
// toggle button. Clicking flips group.collapsed and the contained
// session rows hide.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("group header toggle folds and unfolds session rows", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-fold-group" }),
  });

  try {
    await page.goto(serve.baseUrl);

    const sessionRow = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });

    const groupToggle = page
      .locator('[data-testid="sidebar-group-header"]')
      .first()
      .getByRole("button", { expanded: true })
      .first();
    await groupToggle.click();
    await expect(sessionRow).toBeHidden({ timeout: 5_000 });

    await page
      .locator('[data-testid="sidebar-group-header"]')
      .first()
      .getByRole("button", { expanded: false })
      .first()
      .click();
    await expect(sessionRow).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
