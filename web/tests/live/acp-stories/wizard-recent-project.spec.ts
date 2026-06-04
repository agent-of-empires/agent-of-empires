// User story: pick a project from the Recent tab in the wizard.
//
// Seed a session so the project shows up under Recent. Open the
// wizard, the Recent tab is the default (because hasRecents is true);
// click the row and the wizard records `data.path` and advances.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base("wizard Recent tab selects a known project", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-recent" }),
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();
    await expect(
      page.getByRole("heading", { name: "Project folder", exact: true }),
    ).toBeVisible({ timeout: 10_000 });

    // Recent tab appears only when hasRecents is true (seeded session
    // registered the project under aoe project).
    const recentTab = page.getByRole("button", { name: "Recent" });
    await expect(recentTab).toBeVisible({ timeout: 5_000 });

    // Recent rows render as <button>s containing the project's display
    // name. Scope to the wizard overlay so the sidebar's matching
    // group-header button (rendered behind the overlay) doesn't
    // intercept the click.
    const wizard = page.locator(
      'div.fixed.inset-0.z-50:has(h1:has-text("New session"))',
    );
    const recentRow = wizard
      .locator("button")
      .filter({ hasText: "project" })
      .first();
    await expect(recentRow).toBeVisible({ timeout: 5_000 });
    await recentRow.click();

    await page.getByRole("button", { name: "Next" }).click();

    // Step order: project → session → agent. After a chosen project,
    // Next lands on the Session step.
    await expect(
      page.getByRole("heading", { name: "Name your session", exact: true }),
    ).toBeVisible({ timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
