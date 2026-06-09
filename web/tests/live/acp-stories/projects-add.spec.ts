// User story: add a new project from the Projects view.
//
// Navigate to /projects, click "+ Add project", type a path, set a default
// base branch, click Add. The project appears in the list below with its
// configured base branch.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("add a project from the Projects view", async ({ page }, testInfo) => {
  let projectPath = "";
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      projectPath = join(home, "story-projects-add");
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
    await page.goto(`${serve.baseUrl}/projects`);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: "+ Add project" }).click();
    await page.getByPlaceholder("/path/to/repo").fill(projectPath);
    await page.getByPlaceholder("blank = inherit global default, then auto-detect").fill("develop");
    await page.getByRole("button", { name: "Add", exact: true }).click();

    await expect(page.getByText(projectPath).first()).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByText("story-projects-add", { exact: true }).first()).toBeVisible();
    // The configured base branch persists and renders on the project row.
    await expect(page.getByText("develop", { exact: true }).first()).toBeVisible();
  } finally {
    await serve.stop();
  }
});
