// Live coverage for the nested repo+group sidebar axis (#1720).
//   - Sessions seeded in ONE repo dir across two user groups plus one
//     ungrouped session render, on the "By repo and group" axis, as a
//     single repository block with nested subgroup headers (feature, fix,
//     Ungrouped), each holding its own sessions.
//   - Subgroup collapse is independent of repo collapse and persists, and
//     collapsing the repo header hides every nested subgroup.
//
// The bucketing/split correctness is unit-tested in
// `src/lib/__tests__/sidebarGroups.test.ts`; this spec exercises the real
// server -> nested render + the localStorage-backed axis/collapse toggles.
//
// Seeding runs BEFORE serve spawns so `state.instances` picks up the
// records on boot, mirroring `sidebar-groups-axis.spec.ts`.

import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  resolveAoeBinary,
} from "../helpers/aoeServe";

function seedNestedSessions(sessions: { title: string; group?: string }[]) {
  return ({
    home,
    env,
  }: {
    home: string;
    shimBin: string;
    env: NodeJS.ProcessEnv;
  }) => {
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
      const args = ["add", projectDir, "-t", title, "-c", "claude"];
      if (group) args.push("-g", group);
      const res = spawnSync(binary, args, { env });
      if (res.status !== 0) {
        throw new Error(
          `aoe add failed for ${title}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`,
        );
      }
    }
  };
}

const SEED = seedNestedSessions([
  { title: "feat-one", group: "feature" },
  { title: "feat-two", group: "feature" },
  { title: "fix-one", group: "fix" },
  { title: "loose-one" },
]);

// Click the layers toggle until it reaches the requested axis. The toggle
// cycles repo -> group -> repo+group -> repo, so a bounded loop lands on
// any target without hard-coding the click count.
async function cycleAxisTo(
  toggle: import("@playwright/test").Locator,
  target: string,
) {
  for (let i = 0; i < 3; i++) {
    const current = await toggle.getAttribute("data-axis");
    if (current === target) return;
    await toggle.click();
    // Wait for the axis to actually advance before reading again, so a
    // not-yet-flushed re-render cannot trigger an extra overshooting click.
    await expect(toggle).not.toHaveAttribute("data-axis", current ?? "");
  }
  await expect(toggle).toHaveAttribute("data-axis", target);
}

base.describe("sidebar nested repo+group axis (#1720)", () => {
  base(
    "nests user groups inside the repo block",
    async ({ page }, testInfo) => {
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: SEED,
      });

      try {
        expect(await listSessions(serve.baseUrl)).toHaveLength(4);
        await page.goto(`${serve.baseUrl}/`);

        const axisToggle = page.locator("[data-testid='sidebar-axis-toggle']");
        await expect(axisToggle).toHaveAttribute("data-axis", "repo", {
          timeout: 10_000,
        });
        await cycleAxisTo(axisToggle, "repo+group");

        // One repository block holds all four sessions, split into three
        // nested subgroups: feature, fix, and Ungrouped.
        const repoBlocks = page.locator("[data-testid='sidebar-nested-repo']");
        await expect(repoBlocks).toHaveCount(1);
        const repo = repoBlocks.first();

        await expect(
          repo.locator("[data-testid='sidebar-nested-subgroup']"),
        ).toHaveCount(3);
        await expect(
          repo.locator(
            "[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']",
          ),
        ).toBeVisible();
        await expect(
          repo.locator(
            "[data-testid='sidebar-nested-subgroup'] [data-group-id='fix']",
          ),
        ).toBeVisible();
        await expect(
          repo.locator(
            "[data-testid='sidebar-nested-subgroup'] [data-group-id='__ungrouped__']",
          ),
        ).toBeVisible();

        // Every session stays visible, now nested under its subgroup.
        await expect(
          page.locator("[data-testid='sidebar-session-row']"),
        ).toHaveCount(4);
      } finally {
        await serve.stop();
      }
    },
  );

  base(
    "subgroup collapse is independent of repo collapse and persists",
    async ({ page }, testInfo) => {
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
        await cycleAxisTo(axisToggle, "repo+group");

        const featureSub = page.locator(
          "[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']",
        );
        const featureExpand = featureSub.locator("button[aria-expanded]");
        await expect(featureExpand).toHaveAttribute("aria-expanded", "true");

        // Collapse just the feature subgroup: its rows hide, the fix
        // subgroup's rows stay, and the repo header stays expanded.
        await featureExpand.click();
        await expect(featureExpand).toHaveAttribute("aria-expanded", "false");
        await expect(page.getByText("feat-one")).toBeHidden();
        await expect(page.getByText("fix-one")).toBeVisible();

        // The subgroup collapse survives a reload (per-repo localStorage key).
        await page.reload();
        await expect(axisToggle).toHaveAttribute("data-axis", "repo+group", {
          timeout: 10_000,
        });
        await expect(
          page
            .locator(
              "[data-testid='sidebar-nested-subgroup'] [data-group-id='feature']",
            )
            .locator("button[aria-expanded]"),
        ).toHaveAttribute("aria-expanded", "false");

        // Collapsing the repo header hides every nested subgroup.
        const repoHeader = page
          .locator("[data-testid='sidebar-nested-repo']")
          .first()
          .locator("[data-testid='sidebar-group-header']")
          .first();
        await repoHeader.locator("button[aria-expanded]").click();
        await expect(
          page.locator("[data-testid='sidebar-nested-subgroup']"),
        ).toHaveCount(0);
      } finally {
        await serve.stop();
      }
    },
  );
});
