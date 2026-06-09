// User story: launch a session via the wizard, pressing Cmd/Ctrl+Enter
// on the Review step.
//
// Group-level New session sets skipToReview, so the wizard opens
// directly on the Review step with prefilled values. Inline-edit the
// title via Review's EditableRow, then press the chord; /api/sessions
// returns 201 and a new sidebar row appears.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

base("Cmd/Ctrl+Enter on the Review step creates the session", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-launch-seed" }),
  });

  try {
    await page.goto(serve.baseUrl);
    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    // skipToReview opens the wizard on Review & Launch.
    await expect(page.getByRole("heading", { name: /Review & Launch/i })).toBeVisible({ timeout: 10_000 });

    // Inline-edit the title via Review's EditableRow so the new row is
    // easy to pick out of the sidebar afterwards.
    await page.getByRole("button", { name: /^Title/i }).click();
    const titleInput = page.locator('input[placeholder="Auto-generated"]').first();
    await titleInput.fill("story-launched");
    await titleInput.blur();

    const before = await listSessions(serve.baseUrl);
    await page.keyboard.press(`${MOD}+Enter`);

    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 20_000,
      })
      .toBeGreaterThan(before.length);

    const rows = page.locator('[data-testid="sidebar-session-row"]');
    await expect(rows.filter({ hasText: "story-launched" })).toHaveCount(1, {
      timeout: 15_000,
    });
  } finally {
    await serve.stop();
  }
});
