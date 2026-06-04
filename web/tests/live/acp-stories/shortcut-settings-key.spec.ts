// User story: pressing s on the dashboard opens the Settings view.
//
// Single-key shortcut (useKeyboardShortcuts.ts:96-99). Settings is a
// route, so the spec asserts the URL flips to /settings and a settings
// heading renders.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("s key opens the Settings view", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await expect(
      page.getByRole("button", { name: "Go to dashboard" }),
    ).toBeVisible({ timeout: 10_000 });
    await page.locator("body").click();

    await page.keyboard.press("s");
    await expect(page).toHaveURL(/\/settings/, { timeout: 5_000 });
    // URL flipping is not enough; assert the SettingsView actually
    // rendered so a broken route guard or render error fails loudly.
    // The Profile selector is unique to the Settings page.
    await expect(
      page.getByRole("button", { name: "+ New" }),
    ).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
