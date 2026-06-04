// User story: toggle the "Create a worktree" switch on the wizard's
// session step.
//
// useWorktree defaults to true; toggling it off hides the worktree
// branch input. Toggling back on re-mounts the input.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";
import { wizardScope } from "../../helpers/acp";

base("wizard worktree toggle hides and shows the branch input", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-worktree" }),
  });

  try {
    await page.goto(serve.baseUrl);
    // Use the global New session button (no prefill) so the wizard
    // opens on ProjectStep; group-level prefill skips to Review where
    // the worktree toggle is not rendered.
    await page
      .getByRole("button", { name: "New session", exact: true })
      .first()
      .click();
    const wizard = wizardScope(page);
    await wizard
      .locator("button")
      .filter({ hasText: "project" })
      .first()
      .click();
    await wizard.getByRole("button", { name: "Next" }).click();

    await expect(
      page.getByRole("heading", { name: "Name your session", exact: true }),
    ).toBeVisible({ timeout: 15_000 });

    // #1514 folds the worktree controls behind a top-level "Advanced"
    // disclosure that defaults closed; expand it first.
    await page.getByRole("button", { name: "Advanced" }).click();

    const branchLabel = page.getByText("Branch / worktree name");
    await expect(branchLabel).toBeVisible({ timeout: 10_000 });

    // Click the Toggle switch directly (clicking the wrapping label
    // double-toggles, since both the label and the switch fire onChange).
    const worktreeRow = page
      .locator("label")
      .filter({ hasText: "Create a worktree" });
    const worktreeSwitch = worktreeRow.locator('button[role="switch"]');
    await worktreeSwitch.click();
    await expect(branchLabel).toBeHidden({ timeout: 5_000 });

    await worktreeSwitch.click();
    await expect(branchLabel).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
