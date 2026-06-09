// User story: click the topbar's Toggle sidebar button to hide / show
// the session list.
//
// TopBar.tsx exposes a button with aria-label="Toggle sidebar" that
// flips App.tsx's `sidebarOpen`. Same handler as Cmd/Ctrl+B but
// reached through the mouse.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("topbar Toggle sidebar button hides and shows the session list", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-sidebar-topbar" }),
  });

  try {
    await page.goto(serve.baseUrl);

    const sessionRow = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });

    const toggle = page.getByRole("button", { name: "Toggle sidebar" });
    await toggle.click();
    await expect(sessionRow).toBeHidden({ timeout: 5_000 });

    await toggle.click();
    await expect(sessionRow).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
