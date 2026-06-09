// User story: open the new-session wizard from the sidebar New button
// and close it via the Close button.
//
// Sidebar has aria-label="New session" trigger that opens the wizard.
// Wizard renders an X (aria-label="Close") in its header that
// dismisses the modal.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("sidebar New session opens wizard and X closes it", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);

    // The empty-state sidebar exposes a single "New session" trigger
    // before any groups exist; once groups exist, every group header
    // also has its own. Click the topbar/sidebar primary one.
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    await expect(page.getByRole("heading", { name: "New session", exact: true })).toBeVisible({ timeout: 5_000 });

    await page.getByRole("button", { name: "Close" }).click();
    await expect(page.getByRole("heading", { name: "New session", exact: true })).toBeHidden({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
