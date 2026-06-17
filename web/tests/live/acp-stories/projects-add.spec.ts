// User story: add a project from the sidebar Projects section (#2212).
//
// Click the section's add button, type a path, set a default base branch,
// click Add. The project appears as a no-session row in the Projects section
// with its configured base branch.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base("add a project from the sidebar Projects section", async ({ page }, testInfo) => {
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
    await page.goto(`${serve.baseUrl}/`);

    await page.getByTestId("sidebar-projects-add").click();
    await page.getByPlaceholder("/path/to/repo").fill(projectPath);
    await page.getByPlaceholder("blank = inherit global default, then auto-detect").fill("develop");
    await page.getByRole("button", { name: "Add", exact: true }).click();

    const section = page.getByTestId("sidebar-projects-section");
    const row = section.locator("[data-testid='sidebar-project-row']").filter({ hasText: "story-projects-add" });
    await expect(row).toBeVisible({ timeout: 10_000 });
    // The configured base branch persists and renders on the project row.
    await expect(row.getByText("develop", { exact: false })).toBeVisible();
  } finally {
    await serve.stop();
  }
});
