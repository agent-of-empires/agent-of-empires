// User story: toggle the sound "Enabled" switch and assert state
// persists across reload.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab } from "../../helpers/acp";

base("sound enabled toggle round-trips through the UI", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await openSettingsTab(page, "Sound");

    // ToggleField puts label text in a sibling div, so the role=switch
    // button's accessible name does not include "Enabled". Locate the
    // description text and walk up to the wrapper, then grab the
    // switch button.
    const toggle = page
      .getByText("Play sounds on session status changes")
      .locator(
        'xpath=ancestor::div[contains(@class, "flex") and contains(@class, "items-center") and contains(@class, "justify-between")][1]',
      )
      .locator('button[role="switch"]');
    await expect(toggle).toBeVisible({ timeout: 10_000 });

    const beforeChecked = await toggle.getAttribute("aria-checked");
    await toggle.click();
    const after = beforeChecked === "true" ? "false" : "true";
    await expect(toggle).toHaveAttribute("aria-checked", after);

    await page.reload();
    await openSettingsTab(page, "Sound");
    const reloaded = page
      .getByText("Play sounds on session status changes")
      .locator(
        'xpath=ancestor::div[contains(@class, "flex") and contains(@class, "items-center") and contains(@class, "justify-between")][1]',
      )
      .locator('button[role="switch"]');
    await expect(reloaded).toHaveAttribute("aria-checked", after, {
      timeout: 10_000,
    });
  } finally {
    await serve.stop();
  }
});
