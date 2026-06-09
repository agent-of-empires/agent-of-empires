// User story: change the tmux mouse-support setting and assert it
// persists across reload.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsSelectByLabel, waitForSettingsLoaded } from "../../helpers/acp";

base("tmux mouse SelectField round-trips through the UI", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Tmux");

    const mouse = settingsSelectByLabel(page, "Mouse support");
    await expect(mouse).toBeVisible({ timeout: 10_000 });

    await mouse.selectOption("disabled");
    await expect(mouse).toHaveValue("disabled");

    await page.reload();
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Tmux");
    const reloaded = settingsSelectByLabel(page, "Mouse support");
    await expect(reloaded).toHaveValue("disabled", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
