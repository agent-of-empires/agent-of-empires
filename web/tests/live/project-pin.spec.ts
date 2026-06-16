// Live coverage for pinning a project from the web sidebar (#2047, #2208):
//   - A saved (registered) but UNPINNED project with no sessions does NOT show
//     as a sidebar empty header, yet stays in the registry (Projects view /
//     wizard). Saving a project no longer forces a sidebar header (#2208).
//   - "Pin project" on a populated repo header POSTs /api/projects with
//     pinned:true; the ◆ marker appears and survives a reload.
//   - "Unpin project" PATCHes pinned:false (NOT DELETE): the empty header drops,
//     but the project remains a saved registry entry (the #2208 regression).
//
// The render path is in web/src/components/WorkspaceSidebar.tsx + the merge in
// web/src/lib/registeredProjects.ts; the registry CRUD is
// src/server/api/projects.rs. Live coverage catches wire-format drift the
// mocked specs miss.

import { test as base, expect } from "@playwright/test";
import { join } from "node:path";
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { spawnAoeServe, listSessions, resolveAoeBinary } from "../helpers/aoeServe";

// Seed a session in `projectA` and register `projectB` (a git repo with no
// session). projectB is registered UNPINNED (the `aoe project add` default
// since #2208), so it is a saved project that does NOT show in the sidebar.
function seedSessionAndSavedProject(opts: {
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
    // Register projectB with no session: saved but unpinned by default.
    const reg = spawnSync(resolveAoeBinary(), ["project", "add", projectB, "--scope", "global"], { env });
    if (reg.status !== 0) {
      throw new Error(`aoe project add failed: status=${reg.status} stderr=${reg.stderr?.toString() ?? "<none>"}`);
    }
  };
}

base.describe("pin a project from the web sidebar (#2047, #2208)", () => {
  base("saved-unpinned hidden; pin POSTs; unpin keeps the saved project", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionAndSavedProject({ title: "pin-session" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      const repoA = sessions[0]!.project_path as string;

      await page.goto(`${serve.baseUrl}/`);

      // The populated repo header (projectA) renders from its session.
      const headerA = page.locator(`[data-testid='sidebar-group-header'][data-group-id='${repoA}']`);
      await expect(headerA).toBeVisible({ timeout: 10_000 });

      // projectB is saved but unpinned: it must NOT show as a sidebar header,
      // even though it has no sessions and is in the registry (#2208 story 2).
      await expect(page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "projectB" })).toHaveCount(0);

      // ...but it IS a saved project: the registry still lists it.
      const savedList = await (await page.request.get(`${serve.baseUrl}/api/projects`)).json();
      expect(savedList.some((p: { name: string }) => p.name === "projectB")).toBe(true);

      // ---- Pin projectA from its header menu (POST with pinned:true) ----
      await headerA.click({ button: "right" });
      const pinPost = page.waitForResponse(
        (res) => res.url().endsWith("/api/projects") && res.request().method() === "POST",
      );
      await page.locator("[data-testid='sidebar-group-context-menu-pin']").click();
      const pinRes = await pinPost;
      expect(pinRes.ok()).toBe(true);
      expect(pinRes.request().postDataJSON()).toMatchObject({ path: repoA, scope: "global", pinned: true });

      await expect(headerA.locator("[data-testid='sidebar-group-pinned-marker']")).toBeVisible({ timeout: 5_000 });

      // Registry persisted: reload and the marker is still there.
      await page.reload();
      const headerAReloaded = page.locator(`[data-testid='sidebar-group-header'][data-group-id='${repoA}']`);
      await expect(headerAReloaded.locator("[data-testid='sidebar-group-pinned-marker']")).toBeVisible({
        timeout: 10_000,
      });

      // ---- Unpin projectA: PATCH pinned:false, NOT a DELETE ----
      await headerAReloaded.click({ button: "right" });
      const unpinPatch = page.waitForResponse(
        (res) => res.url().includes("/api/projects/") && res.request().method() === "PATCH",
      );
      await page.locator("[data-testid='sidebar-group-context-menu-unpin']").click();
      const unpinRes = await unpinPatch;
      expect(unpinRes.ok()).toBe(true);
      expect(unpinRes.request().postDataJSON()).toMatchObject({ pinned: false });

      // Marker gone (projectA still has a session, so the header stays).
      await expect(headerAReloaded.locator("[data-testid='sidebar-group-pinned-marker']")).toHaveCount(0, {
        timeout: 5_000,
      });

      // The #2208 regression: unpin must NOT delete the saved project. The
      // registry still lists projectA (it would be in the Projects view / wizard).
      const afterUnpin = await (await page.request.get(`${serve.baseUrl}/api/projects`)).json();
      expect(afterUnpin.some((p: { path: string }) => p.path === repoA)).toBe(true);
    } finally {
      await serve.stop();
    }
  });
});
