// User story: change the update check interval and assert it persists
// across reload.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsNumberInputByLabel, waitForSettingsLoaded } from "../../helpers/acp";

base("update check-interval NumberField round-trips through the UI", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Updates");

    const input = settingsNumberInputByLabel(page, "Check Interval (hours)");
    await expect(input).toBeVisible({ timeout: 10_000 });

    await input.fill("12");
    await input.press("Enter");

    await page.reload();
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Updates");
    const reloaded = settingsNumberInputByLabel(page, "Check Interval (hours)");
    await expect(reloaded).toHaveValue("12", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
