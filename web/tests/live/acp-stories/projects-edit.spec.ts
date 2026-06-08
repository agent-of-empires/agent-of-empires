// User story: edit a project's default base branch from the Projects view.
//
// Add a project with a base branch, click Edit on its row, change the branch
// in the inline editor, Save. The new value renders, and it persists across a
// page reload (proving the PATCH wrote through to the registry).

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base(
  "edit a project's base branch from the Projects view",
  async ({ page }, testInfo) => {
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
        const commit = spawnSync(
          "git",
          ["commit", "--allow-empty", "-q", "-m", "init"],
          {
            cwd: projectPath,
            env: {
              ...env,
              GIT_AUTHOR_NAME: "t",
              GIT_AUTHOR_EMAIL: "t@t",
              GIT_COMMITTER_NAME: "t",
              GIT_COMMITTER_EMAIL: "t@t",
            },
          },
        );
        if (commit.status !== 0) {
          throw new Error(
            `git commit failed: ${commit.stderr?.toString() ?? ""}`,
          );
        }
      },
    });

    try {
      await page.goto(`${serve.baseUrl}/projects`);
      await expect(
        page.getByRole("heading", { name: "Projects", exact: true }),
      ).toBeVisible({ timeout: 10_000 });

      // Register the project with an initial base branch.
      await page.getByRole("button", { name: "+ Add project" }).click();
      await page.getByPlaceholder("/path/to/repo").fill(projectPath);
      await page
        .getByPlaceholder("blank = inherit global default, then auto-detect")
        .fill("develop");
      await page.getByRole("button", { name: "Add", exact: true }).click();
      await expect(
        page.getByText("develop", { exact: true }).first(),
      ).toBeVisible({
        timeout: 5_000,
      });

      // Edit the base branch via the edit modal.
      await page.getByRole("button", { name: "Edit", exact: true }).click();
      const editor = page.getByPlaceholder(
        "blank = inherit global default, then auto-detect",
      );
      await expect(editor).toHaveValue("develop");
      await editor.fill("release");
      await page.getByRole("button", { name: "Save", exact: true }).click();

      await expect(
        page.getByText("release", { exact: true }).first(),
      ).toBeVisible({
        timeout: 5_000,
      });

      // Reload: the change persisted to the registry.
      await page.reload();
      await expect(
        page.getByRole("heading", { name: "Projects", exact: true }),
      ).toBeVisible({ timeout: 10_000 });
      await expect(
        page.getByText("release", { exact: true }).first(),
      ).toBeVisible({
        timeout: 5_000,
      });
    } finally {
      await serve.stop();
    }
  },
);
