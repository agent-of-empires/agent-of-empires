// User story: click the group-level New session button; the wizard
// opens with the project path pre-filled to that group's repo.
//
// WorkspaceSidebar group headers render a per-group "New session in
// <group>" button. Clicking it calls App.tsx's handleCreateSession
// with the repoPath, which sets `wizardPrefill: { path: repoPath }`
// before showing the wizard. The wizard skips the Project step when
// path is preselected and lands on the Agent step.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base(
  "group-level New session prefills the wizard",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-prefill-from-group" }),
    });

    try {
      await page.goto(serve.baseUrl);
      const groupHeader = page
        .locator('[data-testid="sidebar-group-header"]')
        .first();
      await expect(groupHeader).toBeVisible({ timeout: 10_000 });

      const newInGroup = groupHeader.getByRole("button", {
        name: /New session in /i,
      });
      await newInGroup.click();

      // skipToReview lands the wizard directly on the Review & Launch
      // step with the prefill values populated.
      await expect(
        page.getByRole("heading", { name: /Review & Launch/i }),
      ).toBeVisible({ timeout: 10_000 });

      // The Review step's "Project" row renders the seeded repo path
      // (`${home}/project`, per seedSessionViaAoeAdd). Match the row's
      // accessible name ("Project <value>") via getByRole so we do not
      // collide with the StepIndicator's expanded step button (whose
      // accname is just "project", lowercase, no leading slash).
      const projectRow = page.getByRole("button", { name: /^Project \// });
      await expect(projectRow).toContainText("/project", { timeout: 10_000 });
    } finally {
      await serve.stop();
    }
  },
);
