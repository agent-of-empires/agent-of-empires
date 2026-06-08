// User story: open the wizard's Browse tab and see the directory
// browser load.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base(
  "wizard Browse tab renders the directory browser",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });

    try {
      await page.goto(serve.baseUrl);
      await page
        .getByRole("button", { name: "New session", exact: true })
        .first()
        .click();
      await page.getByRole("button", { name: "Browse" }).click();

      await expect(
        page.getByRole("navigation", { name: "Directory path" }),
      ).toBeVisible({ timeout: 10_000 });
      await expect(page.getByPlaceholder("Type to filter...")).toBeVisible();
    } finally {
      await serve.stop();
    }
  },
);
