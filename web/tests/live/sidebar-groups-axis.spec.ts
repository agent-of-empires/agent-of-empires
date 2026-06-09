// Live coverage for the user-defined group axis in the sidebar (#1234).
//   - Three sessions seeded in ONE repo dir but two user groups surface
//     as a single repo group on the default "By repo" axis, then as two
//     group headers (feature, refactor) once the axis toggle flips to
//     "By group".
//   - Collapse state is per-axis and persists: collapsing a group in the
//     group axis survives a reload and does not collapse the repo group.
//
// The split/bucket correctness is unit-tested in
// `src/lib/__tests__/sidebarGroups.test.ts`; this spec exercises the real
// server -> sidebar render + the localStorage-backed axis/collapse toggles.
//
// Seeding runs BEFORE serve spawns so `state.instances` picks up the
// records on boot, mirroring `sidebar-groups.spec.ts`.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";

function seedGroupedSessions(sessions: { title: string; group: string }[]) {
  return ({ home, env }: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => {
    const binary = resolveAoeBinary();
    const projectDir = join(home, "project");
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
    for (const { title, group } of sessions) {
      const res = spawnSync(binary, ["add", projectDir, "-t", title, "-c", "claude", "-g", group], { env });
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

const SEED = seedGroupedSessions([
  { title: "feat-one", group: "feature" },
  { title: "feat-two", group: "feature" },
  { title: "refac-one", group: "refactor" },
]);

base.describe("sidebar user-group axis (#1234)", () => {
  base("axis toggle renders user groups by group_path", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: SEED,
    });

    try {
      expect(await listSessions(serve.baseUrl)).toHaveLength(3);
      await page.goto(`${serve.baseUrl}/`);

      // Default axis is "By repo": all three sessions live in one repo dir,
      // so there is a single repo group and three rows.
      const headers = page.locator("[data-testid='sidebar-group-header']");
      await expect(headers).toHaveCount(1, { timeout: 10_000 });
      await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(3);

      const axisToggle = page.locator("[data-testid='sidebar-axis-toggle']");
      await expect(axisToggle).toHaveAttribute("data-axis", "repo");
      await axisToggle.click();
      await expect(axisToggle).toHaveAttribute("data-axis", "group");

      // Group axis: two headers, keyed by group_path. All three rows stay
      // visible, now nested under their group.
      await expect(headers).toHaveCount(2);
      await expect(page.locator("[data-testid='sidebar-group-header'][data-group-id='feature']")).toBeVisible();
      await expect(page.locator("[data-testid='sidebar-group-header'][data-group-id='refactor']")).toBeVisible();
      await expect(page.locator("[data-testid='sidebar-session-row']")).toHaveCount(3);
    } finally {
      await serve.stop();
    }
  });

  base("group-axis collapse persists across reload and is per-axis", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: SEED,
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      const axisToggle = page.locator("[data-testid='sidebar-axis-toggle']");
      await expect(axisToggle).toHaveAttribute("data-axis", "repo", {
        timeout: 10_000,
      });
      await axisToggle.click();

      const featureHeader = page.locator("[data-testid='sidebar-group-header'][data-group-id='feature']");
      const featureExpand = featureHeader.locator("button[aria-expanded]");
      await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

      await featureExpand.click();
      await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
      await expect(page.getByText("feat-one")).toBeHidden();

      // Reload: the axis choice and the group collapse both restore from
      // localStorage.
      await page.reload();
      await expect(axisToggle).toHaveAttribute("data-axis", "group", {
        timeout: 10_000,
      });
      await expect(featureHeader.locator("button[aria-expanded]")).toHaveAttribute("aria-expanded", "false");

      // Cycling back to the repo axis shows an independent collapse map:
      // the repo group is not collapsed just because a user group was. The
      // toggle now cycles repo -> group -> repo+group -> repo (#1720), so
      // returning to repo from group takes two clicks.
      await axisToggle.click();
      await expect(axisToggle).toHaveAttribute("data-axis", "repo+group");
      await axisToggle.click();
      await expect(axisToggle).toHaveAttribute("data-axis", "repo");
      await expect(page.locator("[data-testid='sidebar-group-header'] button[aria-expanded]")).toHaveAttribute(
        "aria-expanded",
        "true",
      );
    } finally {
      await serve.stop();
    }
  });
});
