// User story: edit a project's default base branch from the sidebar Projects
// section (#2212).
//
// Add a project with a base branch, right-click its row, choose Edit base
// branch, change the value in the modal, Save. The new value renders on the
// row, and it persists across a page reload (proving the PATCH wrote through
// to the registry).

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("edit a project's base branch from the sidebar Projects section", async ({ page }, testInfo) => {
  let projectPath = "";
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      projectPath = join(home, "story-projects-edit");
      mkdirSync(projectPath, { recursive: true });
      const init = spawnSync("git", ["init", "-q"], { cwd: projectPath });
      if (init.status !== 0) {
        throw new Error(`git init failed: ${init.stderr?.toString() ?? ""}`);
      }
      const commit = spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
        cwd: projectPath,
        env: {
          ...env,
          GIT_AUTHOR_NAME: "t",
          GIT_AUTHOR_EMAIL: "t@t",
          GIT_COMMITTER_NAME: "t",
          GIT_COMMITTER_EMAIL: "t@t",
        },
      });
      if (commit.status !== 0) {
        throw new Error(`git commit failed: ${commit.stderr?.toString() ?? ""}`);
      }
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/`);

    // Register the project with an initial base branch via the add form.
    await page.getByTestId("sidebar-projects-add").click();
    await page.getByPlaceholder("/path/to/repo").fill(projectPath);
    await page.getByPlaceholder("blank = inherit global default, then auto-detect").fill("develop");
    await page.getByRole("button", { name: "Add", exact: true }).click();

    const section = page.getByTestId("sidebar-projects-section");
    const row = section.locator("[data-testid='sidebar-project-row']").filter({ hasText: "story-projects-edit" });
    await expect(row).toBeVisible({ timeout: 10_000 });
    await expect(row.getByText("develop", { exact: false })).toBeVisible();

    // Edit the base branch via the row context menu + modal.
    await row.click({ button: "right" });
    await page.getByTestId("sidebar-project-context-menu-edit").click();
    const editor = page.getByPlaceholder("blank = inherit global default, then auto-detect");
    await expect(editor).toHaveValue("develop");
    await editor.fill("release");
    await page.getByRole("button", { name: "Save", exact: true }).click();

    await expect(row.getByText("release", { exact: false })).toBeVisible({ timeout: 5_000 });

    // Reload: the change persisted to the registry.
    await page.reload();
    const rowReloaded = page
      .getByTestId("sidebar-projects-section")
      .locator("[data-testid='sidebar-project-row']")
      .filter({ hasText: "story-projects-edit" });
    await expect(rowReloaded.getByText("release", { exact: false })).toBeVisible({ timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
