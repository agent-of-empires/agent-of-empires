// User story: enabling scratch on ProjectStep hides the worktree
// controls on SessionStep. A scratch directory is not a git repo, so
// the worktree concept does not apply. Closes #1324.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base("scratch hides the worktree section on the Session step", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    await wizard.getByRole("switch", { name: "Skip project folder" }).click();

    await wizard.getByRole("button", { name: "Next" }).click();
    await expect(wizard.getByRole("heading", { name: "Name your session", exact: true })).toBeVisible({
      timeout: 10_000,
    });

    // #1514 folds the worktree section (and, for scratch, the
    // explanatory note that replaces it) behind a top-level "Advanced"
    // disclosure that defaults closed; expand it first.
    await wizard.getByRole("button", { name: "Advanced" }).click();

    // Explanatory note appears in place of the worktree controls.
    await expect(wizard.getByText(/Scratch sessions do not use git worktrees/)).toBeVisible();
    // The "Create a worktree" switch must NOT be in the DOM at all.
    await expect(wizard.getByRole("switch", { name: /Create a worktree/i })).toHaveCount(0);
  } finally {
    await serve.stop();
  }
});
