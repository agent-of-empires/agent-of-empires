// User story: change a tmux setting via the SelectField and confirm
// the new value persists across a reload.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsSelectByLabel, waitForSettingsLoaded } from "../../helpers/acp";

base("tmux status_bar setting select round-trips through the UI", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Tmux");

    const statusBar = settingsSelectByLabel(page, "Status bar");
    await expect(statusBar).toBeVisible({ timeout: 10_000 });

    await statusBar.selectOption("disabled");
    await expect(statusBar).toHaveValue("disabled");

    await page.reload();
    await waitForSettingsLoaded(page);
    await openSettingsTab(page, "Tmux");
    const reloaded = settingsSelectByLabel(page, "Status bar");
    await expect(reloaded).toHaveValue("disabled", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
