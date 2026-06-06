// User story: enter a Group value in the wizard's session step.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("wizard session step records Group", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-group" }),
  });

  try {
    await page.goto(serve.baseUrl);
    // Use the global New session button so the wizard opens on
    // ProjectStep and walks through Session step; the group-level
    // button skips to Review where the Group field is not rendered.
    await page
      .getByRole("button", { name: "New session", exact: true })
      .first()
      .click();
    const wizard = page.locator(
      'div.fixed.inset-0.z-50:has(h1:has-text("New session"))',
    );
    await wizard
      .locator("button")
      .filter({ hasText: "project" })
      .first()
      .click();
    await page.getByRole("button", { name: "Next" }).click();

    await expect(
      page.getByRole("heading", { name: "Name your session", exact: true }),
    ).toBeVisible({ timeout: 15_000 });

    // #1514 folds the Group input behind a top-level "Advanced"
    // disclosure that defaults closed; expand it first.
    await page.getByRole("button", { name: "Advanced" }).click();

    const groupField = page.getByPlaceholder(
      "Optional, for organizing related sessions",
    );
    await expect(groupField).toBeVisible({ timeout: 10_000 });
    await groupField.fill("my-group");
    await expect(groupField).toHaveValue("my-group");
  } finally {
    await serve.stop();
  }
});
