// User story: create a new profile from the settings ProfileSelector.
//
// Navigate to /settings, click "+ New", type a profile name, click
// Create. The new profile lands as an option in the select.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("create a profile from settings ProfileSelector", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);

    const select = page.locator("select").first();
    await expect(select).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: "+ New" }).click();

    const input = page.getByPlaceholder("Profile name");
    await input.fill("story-profile-1");
    await page.getByRole("button", { name: "Create", exact: true }).click();

    await expect(select.locator("option", { hasText: "story-profile-1" })).toHaveCount(1, {
      timeout: 5_000,
    });
  } finally {
    await serve.stop();
  }
});
