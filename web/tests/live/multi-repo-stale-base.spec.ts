// Multi-repo workspace stale-base regression suite (#1511).
//
// Three user stories cover the behavior change in src/git/worktree.rs:
//
//   1. Single-repo, explicit `base_branch` on a fork + upstream layout:
//      the new branch lands on upstream/<base>, not on the stale fork tip.
//
//   2. Multi-repo, explicit `base_branch` where one secondary repo is
//      fork + upstream. The same canonical-remote scoring runs per repo;
//      the secondary's new worktree branches off upstream/<base> too.
//
//   3. Multi-repo, one repo with a misconfigured remote. The create-
//      session response succeeds, but `warnings` includes a per-repo
//      `git fetch ... failed for ...` entry that the wizard pipes to a
//      toast on the client (SessionWizard.tsx:201-204).
//
// Each test drives the live `aoe serve` backend via the REST API. Repos
// are seeded on disk before the server boots (so the daemon picks up
// `extra_repo_paths` on the create call). Worktree HEADs are read with
// `git rev-parse` from the test process; we do not bring in libgit2 on
// the JS side.

import { test as base, expect } from "@playwright/test";
import { spawnSync } from "node:child_process";
import { mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

const GIT_ENV = {
  GIT_AUTHOR_NAME: "t",
  GIT_AUTHOR_EMAIL: "t@t",
  GIT_COMMITTER_NAME: "t",
  GIT_COMMITTER_EMAIL: "t@t",
  // Quiet down init's hint about default branch + advice
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_SYSTEM: "/dev/null",
} as const;

function run(cmd: string, args: string[], cwd: string, extraEnv: Record<string, string> = {}) {
  const res = spawnSync(cmd, args, {
    cwd,
    env: { ...process.env, ...GIT_ENV, ...extraEnv },
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

interface ForkUpstreamLayout {
  /** Path the user passes to `aoe add` (the local clone). */
  localPath: string;
  /** Commit OID at upstream/<branch>'s tip; the fresh canonical version. */
  upstreamTip: string;
  /** Commit OID at origin/<branch>'s tip; the stale fork version. */
  originTip: string;
}

/**
 * Build a fork plus upstream layout under `root`:
 *
 *   <root>/<name>-upstream/   bare repo seeded with commits A and B
 *   <root>/<name>-origin/     bare repo with only commit A (stale fork)
 *   <root>/<name>/            local clone of origin with `upstream` added
 *
 * Returns paths and tip commit OIDs. The local clone is what the user
 * passes to `aoe add` / `extra_repo_paths`.
 */
function seedForkUpstreamLayout(root: string, name: string, branch: string): ForkUpstreamLayout {
  const upstreamDir = join(root, `${name}-upstream`);
  const originDir = join(root, `${name}-origin`);
  const localDir = join(root, name);

  mkdirSync(upstreamDir, { recursive: true });
  mkdirSync(originDir, { recursive: true });

  run("git", ["init", "--bare", "-q", `--initial-branch=${branch}`, upstreamDir], root);
  run("git", ["init", "--bare", "-q", `--initial-branch=${branch}`, originDir], root);

  // Seed both with commit A from a scratch clone of upstream.
  const seedA = join(root, `${name}-seed-a`);
  run("git", ["clone", "-q", upstreamDir, seedA], root);
  writeFileSync(join(seedA, "file.txt"), "hello\n");
  run("git", ["add", "file.txt"], seedA);
  run("git", ["commit", "-q", "-m", "commit A"], seedA, {
    GIT_AUTHOR_DATE: "1700000000 +0000",
    GIT_COMMITTER_DATE: "1700000000 +0000",
  });
  run("git", ["push", "-q", "origin", `HEAD:${branch}`], seedA);
  // Push the same commit to origin so origin/<branch> exists at A too.
  run("git", ["remote", "add", "fork-origin", originDir], seedA);
  run("git", ["push", "-q", "fork-origin", `HEAD:${branch}`], seedA);

  // Add commit B only on upstream.
  writeFileSync(join(seedA, "file2.txt"), "world\n");
  run("git", ["add", "file2.txt"], seedA);
  run("git", ["commit", "-q", "-m", "commit B"], seedA, {
    GIT_AUTHOR_DATE: "1700001000 +0000",
    GIT_COMMITTER_DATE: "1700001000 +0000",
  });
  run("git", ["push", "-q", "origin", `HEAD:${branch}`], seedA);

  const upstreamTip = run("git", ["rev-parse", `${branch}`], seedA);
  const originTip = run("git", ["rev-parse", `fork-origin/${branch}`], seedA);
  expect(upstreamTip).not.toBe(originTip);

  // Now make the local clone the user will use. Clone from origin (the
  // stale fork) and add upstream as a second remote. Mirror the typical
  // developer setup.
  run("git", ["clone", "-q", originDir, localDir], root);
  run("git", ["remote", "add", "upstream", upstreamDir], localDir);
  run("git", ["fetch", "-q", "upstream"], localDir);

  return { localPath: localDir, upstreamTip, originTip };
}

interface CreatedSession {
  id: string;
  warnings?: string[];
  /** Single-repo sessions: present in main_repo_path / branch. */
  branch?: string;
  main_repo_path?: string;
  /** Multi-repo sessions: per-repo worktree_path is the ground-truth
   *  location on disk; do not reconstruct from the template. */
  workspace_repos?: Array<{
    name: string;
    main_repo_path: string;
    worktree_path: string;
  }>;
}

async function createSession(serve: ServeHandle, body: Record<string, unknown>): Promise<CreatedSession> {
  const res = await fetch(`${serve.baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    throw new Error(`POST /api/sessions failed: ${res.status} ${await res.text()}`);
  }
  // Server returns the SessionResponse directly (web/src/lib/api.ts:615
  // wraps it as `{ session }` client-side). Don't wrap here.
  return res.json();
}

/**
 * Resolve the commit OID currently checked out in a worktree directory.
 * Mirrors what `git rev-parse HEAD` would return.
 */
function worktreeHead(worktreePath: string): string {
  return run("git", ["rev-parse", "HEAD"], worktreePath);
}

/**
 * Resolve the workspace directory the server created for a multi-repo
 * session. The default template is `../{branch}-workspace-{session-id}`,
 * but `session-id` here is a fresh UUID minted inside
 * `create_workspace` (src/session/builder.rs:91), not the SessionResponse.id
 * the API returns to the caller. Scan the primary repo's parent for the
 * matching `<branch-slug>-workspace-*` entry instead of trying to
 * reconstruct it from the session id.
 */
function workspaceDir(home: string, branchSlug: string): string {
  const prefix = `${branchSlug}-workspace-`;
  const matches = readdirSync(home).filter((name) => name.startsWith(prefix));
  if (matches.length !== 1) {
    throw new Error(`expected exactly one ${prefix}* entry in ${home}, got: ${JSON.stringify(matches)}`);
  }
  return join(home, matches[0]);
}

function multiRepoWorktreePath(home: string, branchSlug: string, repoName: string): string {
  return join(workspaceDir(home, branchSlug), repoName);
}

base("single-repo: explicit base_branch branches off fresh upstream tip, not stale origin", async ({}, testInfo) => {
  let serve: ServeHandle | undefined;
  try {
    serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home }) => {
        // Seed the fork+upstream layout under HOME so cleanup wipes
        // it along with the rest of the isolated tree.
        seedForkUpstreamLayout(home, "primary", "main");
      },
    });

    // Reach into the home dir we just seeded. The harness exposes it
    // on the handle so the test can compute paths the daemon will
    // accept.
    const layout = {
      localPath: join(serve.home, "primary"),
    };
    const upstreamTip = run("git", ["rev-parse", "upstream/main"], layout.localPath);

    const created = await createSession(serve, {
      path: layout.localPath,
      tool: "claude",
      title: "stale-base-single",
      worktree_branch: "feature/stale-base-single",
      create_new_branch: true,
      base_branch: "main",
    });

    expect(created.warnings ?? []).toEqual([]);

    const worktreePath = join(serve.home, "primary-worktrees", "feature-stale-base-single");
    const head = worktreeHead(worktreePath);
    expect(head).toBe(upstreamTip);
  } finally {
    await serve?.stop();
  }
});

base("multi-repo: secondary repo with fork+upstream layout branches off upstream tip", async ({}, testInfo) => {
  let serve: ServeHandle | undefined;
  try {
    serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home }) => {
        seedForkUpstreamLayout(home, "primary", "main");
        seedForkUpstreamLayout(home, "secondary", "main");
      },
    });

    const primary = join(serve.home, "primary");
    const secondary = join(serve.home, "secondary");
    const primaryUpstream = run("git", ["rev-parse", "upstream/main"], primary);
    const secondaryUpstream = run("git", ["rev-parse", "upstream/main"], secondary);

    const created = await createSession(serve, {
      path: primary,
      tool: "claude",
      title: "stale-base-multi",
      worktree_branch: "feature/stale-base-multi",
      create_new_branch: true,
      base_branch: "main",
      extra_repo_paths: [secondary],
    });

    expect(created.warnings ?? []).toEqual([]);
    expect(created.workspace_repos).toBeDefined();
    expect((created.workspace_repos ?? []).length).toBeGreaterThanOrEqual(2);

    // Each repo's worktree must land on its respective upstream tip.
    // Multi-repo workspaces use the workspace template
    // `../{branch}-workspace-{session-id}` with per-repo subdirs.
    const primaryWorktree = multiRepoWorktreePath(serve.home, "feature-stale-base-multi", "primary");
    const secondaryWorktree = multiRepoWorktreePath(serve.home, "feature-stale-base-multi", "secondary");
    expect(worktreeHead(primaryWorktree)).toBe(primaryUpstream);
    expect(worktreeHead(secondaryWorktree)).toBe(secondaryUpstream);
  } finally {
    await serve?.stop();
  }
});

base("multi-repo: per-repo fetch failure surfaces as warning, session still created", async ({}, testInfo) => {
  let serve: ServeHandle | undefined;
  try {
    serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home }) => {
        // Primary is a healthy repo with one commit.
        const primary = join(home, "primary");
        mkdirSync(primary, { recursive: true });
        run("git", ["init", "-q", "--initial-branch=main", primary], home);
        writeFileSync(join(primary, "file.txt"), "hi\n");
        run("git", ["add", "file.txt"], primary);
        run("git", ["commit", "-q", "-m", "init"], primary);

        // Secondary repo has a misconfigured `origin` remote pointing
        // at a non-existent path. `git fetch origin main` exits
        // non-zero; the new code path surfaces this as a warning
        // instead of failing the session.
        const secondary = join(home, "secondary");
        mkdirSync(secondary, { recursive: true });
        run("git", ["init", "-q", "--initial-branch=main", secondary], home);
        writeFileSync(join(secondary, "file.txt"), "hi\n");
        run("git", ["add", "file.txt"], secondary);
        run("git", ["commit", "-q", "-m", "init"], secondary);
        run("git", ["remote", "add", "origin", join(home, "does-not-exist.git")], secondary);
      },
    });

    const primary = join(serve.home, "primary");
    const secondary = join(serve.home, "secondary");

    const created = await createSession(serve, {
      path: primary,
      tool: "claude",
      title: "fetch-fail",
      worktree_branch: "feature/fetch-fail",
      create_new_branch: true,
      extra_repo_paths: [secondary],
    });

    const warnings = created.warnings ?? [];
    // The warning shape comes from `record_fetch_warning` in
    // src/git/worktree.rs: `git fetch {remote} {branch} failed for
    // {repo}: {detail}`. Pin the format so a future rewording of
    // the warning forces this test to be reconsidered (it's a
    // user-facing string the wizard pipes to a toast).
    const warningPattern = new RegExp(
      `^git fetch \\S+ \\S+ failed for ${secondary.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}: .+`,
    );
    const secondaryWarning = warnings.find((w) => warningPattern.test(w));
    expect(
      secondaryWarning,
      `expected warning matching ${warningPattern} for ${secondary}, got: ${JSON.stringify(warnings)}`,
    ).toBeDefined();

    // Workspace was still created. Both worktree dirs exist.
    const primaryWorktree = multiRepoWorktreePath(serve.home, "feature-fetch-fail", "primary");
    const secondaryWorktree = multiRepoWorktreePath(serve.home, "feature-fetch-fail", "secondary");
    expect(worktreeHead(primaryWorktree)).toBeTruthy();
    expect(worktreeHead(secondaryWorktree)).toBeTruthy();
  } finally {
    await serve?.stop();
  }
});
