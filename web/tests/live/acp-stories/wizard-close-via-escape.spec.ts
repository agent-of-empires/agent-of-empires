// User story: pressing Escape closes the open new-session wizard.
//
// App.tsx wires the global Escape handler to close several overlays;
// the wizard is among them. This story opens the wizard via the
// sidebar New trigger and dismisses with Escape.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base(
  "Escape closes the open new-session wizard",
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
      const wizardHeading = page.getByRole("heading", {
        name: "New session",
        exact: true,
      });
      await expect(wizardHeading).toBeVisible({ timeout: 10_000 });

      await page.keyboard.press("Escape");
      await expect(wizardHeading).toBeHidden({ timeout: 5_000 });
    } finally {
      await serve.stop();
    }
  },
);
