// User story: Cmd/Ctrl+B toggles the workspace sidebar.
//
// Default desktop viewport opens the sidebar (App.tsx:248-250). Press
// Cmd/Ctrl+B and the sidebar's session-row elements should hide; press
// again and they should re-appear. The shortcut binds globally and
// works regardless of focus.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

base("Cmd/Ctrl+B toggles the workspace sidebar", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-sidebar-toggle" }),
  });

  try {
    await page.goto(serve.baseUrl);

    const sessionRow = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });

    await page.keyboard.press(`${MOD}+B`);
    await expect(sessionRow).toBeHidden({ timeout: 5_000 });

    await page.keyboard.press(`${MOD}+B`);
    await expect(sessionRow).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
