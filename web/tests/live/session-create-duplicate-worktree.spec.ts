// Live regression for #1649: a duplicate worktree/branch must surface the
// real collision message, not the generic "Failed to create session".
//
// Drives the create-session HTTP boundary directly (the bug lived in the
// handler, not the wizard UI): POST /api/sessions twice with the same new
// branch against a real git repo. The first creates the worktree; the
// second collides on the precomputed path and must return the informative
// error the frontend already renders verbatim.

import { test, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe } from "../helpers/aoeServe";

test("duplicate worktree branch returns the real collision error, not a generic one", async ({}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, env }) => {
      const projectDir = join(home, "project");
      mkdirSync(projectDir, { recursive: true });
      spawnSync("git", ["init", "-q"], { cwd: projectDir, env });
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
    },
  });

  try {
    const payload = {
      path: join(serve.home, "project"),
      tool: "claude",
      title: "dup-session",
      worktree_branch: "dup-branch",
      create_new_branch: true,
    };

    const first = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    expect(first.status).toBe(201);

    const second = await fetch(`${serve.baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    expect(second.status).toBe(400);

    const body = await second.json();
    expect(body.message).toContain("Worktree already exists");
    expect(body.message).not.toBe("Failed to create session");
  } finally {
    await serve.stop();
  }
});
