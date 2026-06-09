// User story: delete a profile from the ProfileSelector.
//
// Create a profile, select it (so Delete renders since the selected
// profile is not the active/default), accept the confirm() prompt,
// and assert the profile is gone from the select options.
//
// The new profile name must sort AFTER the bootstrap "main" profile.
// `resolve_default_profile` (src/session/config.rs) picks the first
// profile alphabetically when no explicit default is configured. A
// profile that sorts before "main" silently becomes the new default,
// at which point `activeProfile.name === selectedProfile` and the
// ProfileSelector hides its Delete button. Hence `zz-delete-me`.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("delete a profile via ProfileSelector", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    page.on("dialog", (d) => void d.accept());

    await page.goto(`${serve.baseUrl}/settings`);
    const select = page.locator("select").first();
    await expect(select).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: "+ New" }).click();
    await page.getByPlaceholder("Profile name").fill("zz-delete-me");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await expect(select.locator("option", { hasText: "zz-delete-me" })).toHaveCount(1, { timeout: 5_000 });

    await select.selectOption("zz-delete-me");
    await expect(select).toHaveValue("zz-delete-me");
    // Use title="Delete profile" to disambiguate from any other Delete
    // affordance and ensure we click the ProfileSelector Delete button.
    // Delete is conditionally rendered only when selectedProfile is not
    // the default profile, so wait for it explicitly before clicking.
    const deleteBtn = page.getByTitle("Delete profile");
    await expect(deleteBtn).toBeVisible({ timeout: 5_000 });
    await deleteBtn.click();

    await expect(select.locator("option", { hasText: "zz-delete-me" })).toHaveCount(0, { timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
