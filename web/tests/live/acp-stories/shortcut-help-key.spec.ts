// User story: pressing ? on the dashboard opens the Help overlay.
//
// Single-key shortcut, gated to fire only when no input/textarea is
// focused (useKeyboardShortcuts.ts:28-32, :92-95).

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("? key opens the Help overlay", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({ timeout: 10_000 });
    // Make sure focus is on body so the input-gated shortcut fires.
    await page.locator("body").click();

    await page.keyboard.press("Shift+?");
    await expect(page.getByRole("heading", { name: "Help", exact: true })).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
