// Live coverage for the WorkspaceSidebar repo-group surface:
//   - Two sessions seeded in two different repo dirs surface as two
//     groups, each with its session row.
//   - The filter input narrows the visible groups/rows by matching
//     against title, project path, branch, or agent (see
//     `workspaceMatchesFilter` in WorkspaceSidebar.tsx).
//   - Tapping the group header chevron flips `aria-expanded` and hides
//     the row list; tapping again restores it. State lives in
//     `useRepoGroups`'s in-memory + localStorage map.
//
// Pairs with the within-group drag-reorder spec in
// `workspace-ordering.spec.ts`. Drag-into-group is not a real feature
// (drag is constrained to a single group; see #1169).
//
// The session-creation seed runs BEFORE serve spawns so `state.instances`
// picks up both records on boot, mirroring `ensure-session-restart.spec.ts`.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";

function seedTwoRepoSessions(opts: { repoA: { dir: string; title: string }; repoB: { dir: string; title: string } }) {
  return ({ home, env }: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => {
    const binary = resolveAoeBinary();
    for (const { dir, title } of [opts.repoA, opts.repoB]) {
      const projectDir = join(home, dir);
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
      const res = spawnSync(binary, ["add", projectDir, "-t", title, "-c", "claude"], { env });
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

base.describe("sidebar repo groups (#1220)", () => {
  base("two repos render as two groups, both rows visible", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedTwoRepoSessions({
        repoA: { dir: "repo-alpha", title: "alpha-session" },
        repoB: { dir: "repo-beta", title: "beta-session" },
      }),
    });

    try {
      const seeded = await listSessions(serve.baseUrl);
      expect(seeded).toHaveLength(2);

      await page.goto(`${serve.baseUrl}/`);

      // 4-worker cold start can lag past Playwright's 5s assertion
      // default; bump the first paint waits here and in the other two
      // tests in this file.
      const groupHeaders = page.locator("[data-testid='sidebar-group-header']");
      await expect(groupHeaders).toHaveCount(2, { timeout: 10_000 });
      await expect(page.getByText("repo-alpha")).toBeVisible();
      await expect(page.getByText("repo-beta")).toBeVisible();

      const rows = page.locator("[data-testid='sidebar-session-row']");
      await expect(rows).toHaveCount(2);
      await expect(page.getByText("alpha-session")).toBeVisible();
      await expect(page.getByText("beta-session")).toBeVisible();
    } finally {
      await serve.stop();
    }
  });

  base("filter input narrows visible groups + rows by repo name", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedTwoRepoSessions({
        repoA: { dir: "repo-alpha", title: "alpha-session" },
        repoB: { dir: "repo-beta", title: "beta-session" },
      }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(2, { timeout: 10_000 });

      await page.getByLabel("Filter sessions").click();
      const filter = page.locator("[data-testid='sidebar-filter-input']");
      await expect(filter).toBeVisible();

      await filter.fill("alpha");
      await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(1);
      await expect(page.getByText("repo-alpha")).toBeVisible();
      await expect(page.getByText("repo-beta")).toBeHidden();

      // Clearing the input restores both groups; we drive the same input
      // rather than toggling the filter off because the toggle button
      // ALSO clears the query, which would hide the input we'd want to
      // assert on.
      await filter.fill("");
      await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(2);

      // No-match query renders the empty-state placeholder.
      await filter.fill("nonexistent-repo-xyz");
      await expect(page.getByText(/No matches for/)).toBeVisible();
      await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(0);
    } finally {
      await serve.stop();
    }
  });

  base("group header chevron toggles aria-expanded and hides rows", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedTwoRepoSessions({
        repoA: { dir: "repo-alpha", title: "alpha-session" },
        repoB: { dir: "repo-beta", title: "beta-session" },
      }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      const alphaHeader = page.locator("[data-testid='sidebar-group-header']", { has: page.getByText("repo-alpha") });
      const expandBtn = alphaHeader.locator("button[aria-expanded]");
      await expect(expandBtn).toHaveAttribute("aria-expanded", "true", {
        timeout: 10_000,
      });

      await expandBtn.click();
      await expect(expandBtn).toHaveAttribute("aria-expanded", "false");

      // Collapsing the alpha group hides its row but leaves beta's row
      // (in the other group) untouched.
      await expect(page.getByText("alpha-session")).toBeHidden();
      await expect(page.getByText("beta-session")).toBeVisible();

      await expandBtn.click();
      await expect(expandBtn).toHaveAttribute("aria-expanded", "true");
      await expect(page.getByText("alpha-session")).toBeVisible();
    } finally {
      await serve.stop();
    }
  });
});
