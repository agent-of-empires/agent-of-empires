// User story: open the About modal from the topbar overflow menu.
//
// TopBar.tsx registers an "About" entry in OverflowMenu; selecting it
// flips App.tsx's `showAbout` state and AboutModal renders with
// role="dialog" + aria-labelledby="about-modal-title". Closing via the
// X (aria-label="Close") unmounts it.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("About modal opens from overflow menu and closes via the X", async ({ page }, testInfo) => {
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
    await expect(dialog.getByText("Agent of Empires")).toBeVisible();

    await dialog.getByRole("button", { name: "Close" }).click();
    await expect(dialog).toBeHidden({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
