// User story: enabling the scratch toggle makes the wizard's Next
// button activate without picking a project path. The Recent/Browse/
// Clone-URL tab strip and the directory browser disappear so the user
// has no path source to choose. Closes #1324.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base("scratch toggle enables Next without a path", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    // Baseline: Next is disabled because no path is selected.
    const nextButton = wizard.getByRole("button", { name: "Next" });
    await expect(nextButton).toBeDisabled();

    // Flip the toggle. The reducer also clears any prefilled path /
    // useWorktree state, so Next must transition to enabled.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();

    await expect(nextButton).toBeEnabled({ timeout: 5_000 });

    // The scratch confirmation card replaces the path picker.
    await expect(wizard.getByText(/Scratch session/)).toBeVisible();
    // The Browse tab button must NOT be visible while scratch is on.
    await expect(wizard.getByRole("button", { name: "Browse" })).toBeHidden();
  } finally {
    await serve.stop();
  }
});
