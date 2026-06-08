// User story: rename an existing profile via the ProfileSelector.
//
// Create a profile, click Rename, type new name, click Rename to
// commit. Asserts the renamed profile appears in the select.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("rename a profile via ProfileSelector", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    const select = page.locator("select").first();
    await expect(select).toBeVisible({ timeout: 10_000 });

    // Seed with a fresh profile so the default profile is not the target.
    await page.getByRole("button", { name: "+ New" }).click();
    await page.getByPlaceholder("Profile name").fill("rename-source");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await expect(
      select.locator("option", { hasText: "rename-source" }),
    ).toHaveCount(1, {
      timeout: 5_000,
    });

    // ProfileSelector's parent toggles `selectedProfile` on select
    // change; only when selected != activeProfile.name does the Delete
    // button render. To rename, click into the freshly-created profile
    // first so renaming targets it (the activeProfile defaults to
    // "default" and the Rename button always renders for the selected
    // profile here).
    await select.selectOption("rename-source");

    await page.getByRole("button", { name: "Rename" }).click();
    const input = page.getByPlaceholder("New name");
    await input.fill("rename-target");
    await page.getByRole("button", { name: "Rename", exact: true }).click();

    await expect(
      select.locator("option", { hasText: "rename-target" }),
    ).toHaveCount(1, {
      timeout: 5_000,
    });
    await expect(
      select.locator("option", { hasText: "rename-source" }),
    ).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});
