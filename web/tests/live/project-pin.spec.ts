// Live coverage for pinning a project from the web sidebar (#2047):
//   - A registered project with no sessions shows as an empty header in the
//     sidebar (the ◆ marker + a New session button), parity with the TUI.
//   - "Pin project" on a populated repo header POSTs /api/projects and the
//     ◆ marker appears and survives a reload (registry persistence).
//   - "Unpin project" on the empty project DELETEs /api/projects and its
//     header drops from the sidebar (it had no sessions keeping it alive).
//
// The render path is in web/src/components/WorkspaceSidebar.tsx + the merge
// in web/src/lib/registeredProjects.ts; the registry CRUD is
// src/server/api/projects.rs. Live coverage catches wire-format drift the
// mocked specs miss.

import { test as base, expect } from "@playwright/test";
import { join } from "node:path";
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";

// Seed a session in `projectA` and register `projectB` (a git repo with no
// session) so the sidebar shows one populated header and one pinned-but-empty
// header. Runs before serve spawns so the in-memory caches pick both up.
function seedSessionAndEmptyProject(opts: {
  title: string;
}): (seedEnv: { home: string; shimBin: string; env: NodeJS.ProcessEnv }) => void {
  return ({ home, env }) => {
    const gitEnv = {
      ...env,
      GIT_AUTHOR_NAME: "t",
      GIT_AUTHOR_EMAIL: "t@t",
      GIT_COMMITTER_NAME: "t",
      GIT_COMMITTER_EMAIL: "t@t",
    };
    const runGit = (args: string[], dir: string) => {
      const res = spawnSync("git", args, { cwd: dir, env: gitEnv });
      if (res.error || res.status !== 0) {
        throw new Error(
          `git ${args.join(" ")} failed in ${dir}: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"} error=${res.error?.message ?? "<none>"}`,
        );
      }
    };
    const initRepo = (dir: string) => {
      mkdirSync(dir, { recursive: true });
      runGit(["init", "-q"], dir);
      runGit(["commit", "--allow-empty", "-q", "-m", "init"], dir);
    };

    const projectA = join(home, "projectA");
    const projectB = join(home, "projectB");
    initRepo(projectA);
    initRepo(projectB);

    const add = spawnSync(resolveAoeBinary(), ["add", projectA, "-t", opts.title, "-c", "claude"], { env });
    if (add.status !== 0) {
      throw new Error(`aoe add failed: status=${add.status} stderr=${add.stderr?.toString() ?? "<none>"}`);
    }
    // Register projectB with no session: the pinned-but-empty case.
    const reg = spawnSync(resolveAoeBinary(), ["project", "add", projectB, "--scope", "global"], { env });
    if (reg.status !== 0) {
      throw new Error(`aoe project add failed: status=${reg.status} stderr=${reg.stderr?.toString() ?? "<none>"}`);
    }
  };
}

base.describe("pin a project from the web sidebar (#2047)", () => {
  base("empty project shows, pin persists, unpin removes", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionAndEmptyProject({ title: "pin-session" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      const repoA = sessions[0]!.project_path as string;

      await page.goto(`${serve.baseUrl}/`);

      // The populated repo header (projectA) renders from its session.
      const headerA = page.locator(`[data-testid='sidebar-group-header'][data-group-id='${repoA}']`);
      await expect(headerA).toBeVisible({ timeout: 10_000 });

      // The pinned-but-empty project (projectB) renders despite no sessions,
      // with the ◆ marker and a New session button.
      const headerB = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "projectB" });
      await expect(headerB).toBeVisible({ timeout: 10_000 });
      await expect(headerB.locator("[data-testid='sidebar-group-pinned-marker']")).toBeVisible();
      await expect(headerB.locator("[aria-label='New session in projectB']")).toBeVisible();

      // ---- Pin projectA from its header menu ----
      await headerA.click({ button: "right" });
      const pinPost = page.waitForResponse(
        (res) => res.url().endsWith("/api/projects") && res.request().method() === "POST",
      );
      await page.locator("[data-testid='sidebar-group-context-menu-pin']").click();
      const pinRes = await pinPost;
      expect(pinRes.ok()).toBe(true);
      expect(pinRes.request().postDataJSON()).toMatchObject({ path: repoA, scope: "global" });

      await expect(headerA.locator("[data-testid='sidebar-group-pinned-marker']")).toBeVisible({ timeout: 5_000 });

      // Registry persisted: reload and the marker is still there.
      await page.reload();
      const headerAReloaded = page.locator(`[data-testid='sidebar-group-header'][data-group-id='${repoA}']`);
      await expect(headerAReloaded.locator("[data-testid='sidebar-group-pinned-marker']")).toBeVisible({
        timeout: 10_000,
      });

      // ---- Unpin the empty projectB: its header drops (no sessions) ----
      const headerBReloaded = page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "projectB" });
      await headerBReloaded.click({ button: "right" });
      const unpinDelete = page.waitForResponse(
        (res) => res.url().includes("/api/projects/") && res.request().method() === "DELETE",
      );
      await page.locator("[data-testid='sidebar-group-context-menu-unpin']").click();
      const unpinRes = await unpinDelete;
      expect(unpinRes.ok()).toBe(true);

      await expect(page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "projectB" })).toHaveCount(
        0,
        { timeout: 10_000 },
      );
    } finally {
      await serve.stop();
    }
  });
});
