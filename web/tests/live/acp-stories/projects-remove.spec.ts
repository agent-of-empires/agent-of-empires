// User story: remove a project from the sidebar Projects section (#2212).
//
// Right-clicking a project row offers "Remove project", which triggers a
// window confirm() then calls DELETE on every registration for the path. The
// row disappears after the confirmation passes.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, resolveAoeBinary } from "../../helpers/aoeServe";

base("remove a project from the sidebar Projects section", async ({ page }, testInfo) => {
  let projectPath = "";
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      projectPath = join(home, "story-projects-remove");
      mkdirSync(projectPath, { recursive: true });
      const initRes = spawnSync("git", ["init", "-q"], {
        cwd: projectPath,
        env,
      });
      if (initRes.status !== 0) {
        throw new Error(`git init failed: status=${initRes.status} stderr=${initRes.stderr?.toString() ?? "<none>"}`);
      }
      const commitRes = spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
        cwd: projectPath,
        env: {
          ...env,
          GIT_AUTHOR_NAME: "t",
          GIT_AUTHOR_EMAIL: "t@t",
          GIT_COMMITTER_NAME: "t",
          GIT_COMMITTER_EMAIL: "t@t",
        },
      });
      if (commitRes.status !== 0) {
        throw new Error(
          `git commit failed: status=${commitRes.status} stderr=${commitRes.stderr?.toString() ?? "<none>"}`,
        );
      }
      const res = spawnSync(resolveAoeBinary(), ["project", "add", projectPath], { env });
      if (res.status !== 0) {
        throw new Error(`aoe project add failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
      }
    },
  });

  try {
    page.on("dialog", (d) => void d.accept());

    await page.goto(`${serve.baseUrl}/`);
    const row = page
      .getByTestId("sidebar-projects-section")
      .locator("[data-testid='sidebar-project-row']")
      .filter({ hasText: "story-projects-remove" });
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.click({ button: "right" });
    await page.getByTestId("sidebar-project-context-menu-remove").click();
    await expect(row).toHaveCount(0, { timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
