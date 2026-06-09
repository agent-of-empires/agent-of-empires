// User story: edit the session title via the wizard's Review step
// inline edit affordance.
//
// Group click opens the wizard with skipToReview; the Title row in
// Review is an EditableRow that flips to an inline input on click.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("wizard records the title from Review's inline editor", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-title" }),
  });

  try {
    await page.goto(serve.baseUrl);
    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    await expect(page.getByRole("heading", { name: /Review & Launch/i })).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: /^Title/i }).click();
    const titleInput = page.locator('input[placeholder="Auto-generated"]').first();
    await expect(titleInput).toBeVisible({ timeout: 5_000 });
    await titleInput.fill("my-session-title");
    await expect(titleInput).toHaveValue("my-session-title");
    await titleInput.blur();

    await expect(page.getByText("my-session-title").first()).toBeVisible({
      timeout: 5_000,
    });
  } finally {
    await serve.stop();
  }
});
