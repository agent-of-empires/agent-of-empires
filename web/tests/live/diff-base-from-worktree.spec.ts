// Worktree base branch sets the diff viewer's default comparison (#1951).
//
// When a worktree is created off a base branch X, the diff endpoint must
// default its comparison to X (recorded on worktree_info.base_branch),
// not to the repo's auto-detected default. An explicit per-session
// override still wins, and clearing the override falls back to the
// worktree base again.
//
// Drives the live `aoe serve` backend over REST. The repo is seeded on
// disk before the server boots so the daemon picks it up on the create
// call. The default-branch auto-detection would resolve to `main`, so an
// observed base of `release` proves the worktree-base layer engaged.

import { test as base, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

const GIT_ENV = {
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@t",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@t",
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_SYSTEM: "/dev/null",
} as const;

function run(cmd: string, args: string[], cwd: string) {
  const res = spawnSync(cmd, args, {
    cwd,
    env: { ...process.env, ...GIT_ENV },
    encoding: "utf8",
  });
  if (res.error || res.status !== 0) {
    const errMsg = res.error ? String(res.error) : "non-zero exit";
    throw new Error(
      `${cmd} ${args.join(" ")} failed in ${cwd}: ${errMsg}; status=${res.status}\nstdout=${res.stdout}\nstderr=${res.stderr}`,
    );
  }
  return res.stdout.trim();
}

interface RepoBase {
  repo_name: string | null;
  base_branch: string;
}

async function diffBases(serve: ServeHandle, id: string): Promise<RepoBase[]> {
  const res = await fetch(`${serve.baseUrl}/api/sessions/${id}/diff/files`);
  if (!res.ok) {
    throw new Error(`GET diff/files failed: ${res.status} ${await res.text()}`);
  }
  const body = (await res.json()) as { per_repo_bases: RepoBase[] };
  return body.per_repo_bases;
}

async function setDiffBase(serve: ServeHandle, id: string, baseBranch: string) {
  const res = await fetch(`${serve.baseUrl}/api/sessions/${id}/diff-base`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ base_branch: baseBranch }),
  });
  if (!res.ok) {
    throw new Error(
      `PATCH diff-base failed: ${res.status} ${await res.text()}`,
    );
  }
}

base(
  "diff base defaults to the worktree's base branch, override still wins",
  async ({}, testInfo) => {
    let serve: ServeHandle | undefined;
    try {
      serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: ({ home }) => {
          // Healthy single repo: `main` with two commits, plus a
          // `release` branch pinned at the first commit. Auto-detection
          // resolves to `main`, so a later base of `release` can only
          // come from the worktree base layer.
          const primary = join(home, "primary");
          run("git", ["init", "-q", "--initial-branch=main", primary], home);
          writeFileSync(join(primary, "file.txt"), "hello\n");
          run("git", ["add", "file.txt"], primary);
          run("git", ["commit", "-q", "-m", "commit A"], primary);
          run("git", ["branch", "release"], primary);
          writeFileSync(join(primary, "file2.txt"), "world\n");
          run("git", ["add", "file2.txt"], primary);
          run("git", ["commit", "-q", "-m", "commit B"], primary);
        },
      });

      const primary = join(serve.home, "primary");

      const res = await fetch(`${serve.baseUrl}/api/sessions`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          path: primary,
          tool: "claude",
          title: "diff-base-from-worktree",
          worktree_branch: "feature/diff-base-from-worktree",
          create_new_branch: true,
          base_branch: "release",
        }),
      });
      if (!res.ok) {
        throw new Error(
          `POST /api/sessions failed: ${res.status} ${await res.text()}`,
        );
      }
      const created = (await res.json()) as { id: string };

      // No override set: the diff base follows the worktree's recorded
      // base branch, not the auto-detected `main`.
      const defaultBases = await diffBases(serve, created.id);
      expect(defaultBases.length).toBe(1);
      expect(defaultBases[0].base_branch).toBe("release");

      // An explicit per-session override outranks the worktree base.
      await setDiffBase(serve, created.id, "main");
      const overriddenBases = await diffBases(serve, created.id);
      expect(overriddenBases[0].base_branch).toBe("main");

      // Clearing the override falls back to the worktree base again.
      await setDiffBase(serve, created.id, "");
      const clearedBases = await diffBases(serve, created.id);
      expect(clearedBases[0].base_branch).toBe("release");
    } finally {
      await serve?.stop();
    }
  },
);
