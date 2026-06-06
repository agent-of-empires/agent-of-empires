// User story: every scratch session lives under its own
// `<app_dir>/scratch/<id>/` directory, so bucketing the sidebar by
// `project_path` would render N one-session groups for N scratch
// sessions. The fix (in `useRepoGroups`) collapses them all into one
// synthetic "Scratch" group, mirroring the existing multi-repo group.
// This spec seeds two scratch sessions + one regular session and
// asserts the sidebar renders exactly two groups: the real repo on top
// and a single "Scratch" group at the bottom holding both scratch
// rows. Closes #1324 follow-up.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../helpers/aoeServe";

function seedScratchAndRepoSessions() {
  return ({
    home,
    env,
  }: {
    home: string;
    shimBin: string;
    env: NodeJS.ProcessEnv;
  }) => {
    const binary = resolveAoeBinary();

    // One real repo session: gives the sidebar a non-scratch group to
    // contrast against the synthetic Scratch group.
    const repoDir = join(home, "repo-alpha");
    mkdirSync(repoDir, { recursive: true });
    spawnSync("git", ["init", "-q"], { cwd: repoDir });
    spawnSync("git", ["commit", "--allow-empty", "-q", "-m", "init"], {
      cwd: repoDir,
      env: {
        ...env,
        GIT_AUTHOR_NAME: "t",
        GIT_AUTHOR_EMAIL: "t@t",
        GIT_COMMITTER_NAME: "t",
        GIT_COMMITTER_EMAIL: "t@t",
      },
    });
    const repoRes = spawnSync(
      binary,
      ["add", repoDir, "-t", "alpha-session", "-c", "claude"],
      { env },
    );
    if (repoRes.status !== 0) {
      throw new Error(
        `aoe add (repo) failed: status=${repoRes.status} stderr=${repoRes.stderr?.toString() ?? "<none>"}`,
      );
    }

    // Two scratch sessions: each gets its own `scratch/<id>/` dir, so
    // a naive grouping by project_path would render two scratch
    // groups. The fix is to bucket them into one synthetic group.
    for (const title of ["scratch-one", "scratch-two"]) {
      const res = spawnSync(
        binary,
        ["add", "--scratch", "-t", title, "-c", "claude"],
        { env },
      );
      if (res.status !== 0) {
        throw new Error(
          `aoe add --scratch (${title}) failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

base(
  "scratch sessions render in a single synthetic Scratch group",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedScratchAndRepoSessions(),
    });

    try {
      // Sanity: three sessions seeded, two of them scratch.
      const seeded = await listSessions(serve.baseUrl);
      expect(seeded).toHaveLength(3);
      expect(seeded.filter((s) => s.scratch)).toHaveLength(2);

      await page.goto(`${serve.baseUrl}/`);

      // Two groups: the real repo (alpha) + the synthetic "Scratch"
      // group. If grouping regressed, each scratch session would
      // surface as its own header and this assertion would see 3.
      const groupHeaders = page.locator("[data-testid='sidebar-group-header']");
      await expect(groupHeaders).toHaveCount(2, { timeout: 10_000 });

      // Both real and synthetic group labels are visible. The synthetic
      // bucket is identified by its stable group id (`__scratch__`)
      // rather than by a text match, because the header's accessible
      // text node sits inside nested elements and a substring/regex
      // text filter is brittle under truncation / layout reflow.
      await expect(page.getByText("repo-alpha")).toBeVisible();
      const scratchHeader = page.locator(
        "[data-testid='sidebar-group-header'][data-group-id='__scratch__']",
      );
      await expect(scratchHeader).toBeVisible();
      await expect(scratchHeader).toContainText("Scratch");

      // All three session rows are visible: alpha on top, both scratch
      // sessions under the synthetic group. Row count proves no rows
      // got dropped by the grouping change.
      const rows = page.locator("[data-testid='sidebar-session-row']");
      await expect(rows).toHaveCount(3);
      await expect(page.getByText("alpha-session")).toBeVisible();
      await expect(page.getByText("scratch-one")).toBeVisible();
      await expect(page.getByText("scratch-two")).toBeVisible();
    } finally {
      await serve.stop();
    }
  },
);
