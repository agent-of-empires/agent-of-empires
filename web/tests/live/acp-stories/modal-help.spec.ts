// User story: open the Help overlay from the topbar overflow menu.
//
// TopBar.tsx registers a "Help" entry that flips App.tsx's `showHelp`
// state. HelpOverlay renders a centered card with an "Help" heading
// and a list of keyboard bindings (Cmd+K, ?, Esc, etc.).

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("Help overlay opens from overflow menu", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);

    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Help" }).click();

    await expect(
      page.getByRole("heading", { name: "Help", exact: true }),
    ).toBeVisible({ timeout: 5_000 });
    // Sample bindings rendered from the overlay's shortcuts list.
    await expect(page.getByText(/Toggle this help/i)).toBeVisible();
  } finally {
    await serve.stop();
  }
});
