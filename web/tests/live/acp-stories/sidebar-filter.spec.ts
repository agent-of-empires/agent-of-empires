// User story: filter sessions via the sidebar filter input.
//
// Click the filter button (aria-label="Filter sessions"), type into
// the input (data-testid="sidebar-filter-input"), and the session list
// should narrow to titles matching the query.

import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, resolveAoeBinary } from "../../helpers/aoeServe";

function seedTwoSessions(): (seedEnv: {
  home: string;
  shimBin: string;
  env: NodeJS.ProcessEnv;
}) => void {
  return ({ home, env }) => {
    for (const [title, subdir] of [
      ["alpha-search", "project-a"],
      ["bravo-other", "project-b"],
    ] as const) {
      const projectDir = join(home, subdir);
      mkdirSync(projectDir, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: projectDir });
      spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
        cwd: projectDir,
        env: {
          ...env,
          GIT_AUTHOR_NAME: "t",
          GIT_AUTHOR_EMAIL: "t@t",
          GIT_COMMITTER_NAME: "t",
          GIT_COMMITTER_EMAIL: "t@t",
        },
      });
      const res = spawnSync(
        resolveAoeBinary(),
        ["add", projectDir, "-t", title, "-c", "claude"],
        { env },
      );
      if (res.status !== 0) {
        throw new Error(
          `aoe add ${title} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

base("sidebar filter input narrows session rows", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedTwoSessions(),
  });

  try {
    await page.goto(serve.baseUrl);

    await expect(page.getByText("alpha-search")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("bravo-other")).toBeVisible();

    await page.getByRole("button", { name: "Filter sessions" }).click();
    const filter = page.locator('[data-testid="sidebar-filter-input"]');
    await expect(filter).toBeVisible();
    await filter.fill("alpha");

    await expect(page.getByText("alpha-search")).toBeVisible({ timeout: 5_000 });
    await expect(page.getByText("bravo-other")).toBeHidden({ timeout: 5_000 });

    await filter.fill("");
    await expect(page.getByText("bravo-other")).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
