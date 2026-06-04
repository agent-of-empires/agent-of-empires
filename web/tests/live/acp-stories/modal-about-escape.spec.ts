// User story: pressing Escape closes the About modal.
//
// App.tsx's global Escape handler calls setShowAbout(false) so a
// single keystroke dismisses the dialog from any focus location.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("Escape closes the About modal", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();

    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
