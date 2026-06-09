// User story: open the new-session wizard and verify the ProjectStep
// surfaces the Browse + Clone URL tabs (Recent shows only when there
// are recent projects).

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("wizard ProjectStep exposes Browse and Clone URL tabs", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();
    await expect(page.getByRole("heading", { name: "Project folder", exact: true })).toBeVisible({ timeout: 10_000 });

    await expect(page.getByRole("button", { name: "Browse" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Clone URL", exact: true })).toBeVisible();

    await page.getByRole("button", { name: "Clone URL", exact: true }).click();
    await expect(page.getByPlaceholder("https://github.com/user/repo.git")).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
