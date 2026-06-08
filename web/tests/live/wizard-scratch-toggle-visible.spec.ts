// User story: opening the wizard shows a "Skip project folder" toggle
// above the project-source tab bar so users can opt out of picking a
// project path. Closes #1324.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base(
  "wizard scratch toggle is visible above the project tabs",
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

      const wizard = page.locator(
        'div.fixed.inset-0.z-50:has(h1:has-text("New session"))',
      );
      await expect(wizard).toBeVisible({ timeout: 15_000 });

      // The scratch toggle uses the existing role="switch" pattern and
      // carries the aria-label set in `ProjectStep.tsx`.
      const toggle = wizard.getByRole("switch", {
        name: "Skip project folder",
      });
      await expect(toggle).toBeVisible({ timeout: 10_000 });
      await expect(toggle).toHaveAttribute("aria-checked", "false");
    } finally {
      await serve.stop();
    }
  },
);
