// User story: while a new session is being created, the wizard shows
// a "Creating session..." banner on the Launch button.
//
// SessionWizard dispatches SUBMIT_START before the POST /api/sessions
// resolves, flipping isSubmitting=true. ReviewStep then swaps the
// button copy to "Creating session..." with a spinner. We delay the
// POST via page.route() so the banner is observable.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base("Creating session banner appears while POST is in flight", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-banner-seed" }),
  });

  try {
    // Delay the create-session POST so the banner is observable.
    await page.route("**/api/sessions", async (route) => {
      if (route.request().method() !== "POST") {
        await route.continue();
        return;
      }
      await new Promise((r) => setTimeout(r, 1500));
      await route.continue();
    });

    await page.goto(serve.baseUrl);
    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    // skipToReview lands the wizard on the Review step with prefilled
    // values; click Launch directly.
    await expect(
      page.getByRole("heading", { name: /Review & Launch/i }),
    ).toBeVisible({ timeout: 10_000 });

    const launchButton = page.getByRole("button", { name: /Launch session/i });
    await launchButton.click();

    await expect(page.getByText("Creating session...")).toBeVisible({
      timeout: 5_000,
    });
  } finally {
    await serve.stop();
  }
});
