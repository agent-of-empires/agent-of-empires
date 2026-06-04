// User story: pressing n on the dashboard opens the new-session
// wizard.
//
// Single-key shortcut (useKeyboardShortcuts.ts:84-87). The wizard
// renders an h1 "New session" header in SessionWizard.tsx:245.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("n key opens the new-session wizard", async ({ page }, testInfo) => {
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

    await page.keyboard.press("n");
    await expect(
      page.getByRole("heading", { name: "New session", exact: true }),
    ).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
