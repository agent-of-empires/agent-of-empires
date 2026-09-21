// Repository handling against a real server: clone from the wizard, worktree base branches, multi-repo fetches.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { createBareRepo, createSeededBareRepo, gitEnv } from "../helpers/gitFixture";

function run(env: NodeJS.ProcessEnv, args: string[], cwd: string, extraEnv: Record<string, string> = {}) {
  const res = spawnSync("git", args, { cwd, env: { ...gitEnv(env), ...extraEnv }, encoding: "utf8" });
  if (res.error || res.status !== 0) {
    throw new Error(
      `git ${args.join(" ")} failed in ${cwd}: ${res.error ?? "non-zero exit"}; status=${res.status}\nstdout=${res.stdout}\nstderr=${res.stderr}`,
    );
  }
  return res.stdout.trim();
}

/** `~/<name>` on `main` with one commit per file, in order. */
function seedRepo(env: NodeJS.ProcessEnv, home: string, name: string, files: string[]) {
  const dir = join(home, name);
  run(env, ["init", "-q", "--initial-branch=main", dir], home);
  for (const file of files) {
    writeFileSync(join(dir, file), `${file}\n`);
    run(env, ["add", file], dir);
    run(env, ["commit", "-q", "-m", `add ${file}`], dir);
  }
  return dir;
}

async function createSession(serve: ServeHandle, body: Record<string, unknown>) {
  const res = await fetch(`${serve.baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`POST /api/sessions failed: ${res.status} ${await res.text()}`);
  return (await res.json()) as { id: string; warnings?: string[]; workspace_repos?: unknown[] };
}

test.describe("clone URL tab", () => {
  async function openCloneTab(page: Page, serve: ServeHandle) {
    await page.goto(serve.baseUrl);
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: "Clone URL", exact: true }).click();
  }
  const launchButton = (page: Page) =>
    page.getByTestId("session-wizard").getByRole("button", { name: /Launch session/ });

  test("clone happy path: file:// URL clones into HOME and the wizard advances", async ({ page, serve }) => {
    const bare = createBareRepo(serve.home, serve.env);
    await openCloneTab(page, serve);
    const cloneBtn = page.getByRole("button", { name: "Clone repository" });
    await expect(cloneBtn).toBeDisabled();
    await page.locator("#clone-url").fill(bare.url);
    await expect(cloneBtn).toBeEnabled();
    // A pinned destination keeps the assertion independent of repo-name derivation.
    await page.getByRole("button", { name: /Advanced/ }).click();
    const dest = join(serve.home, "cloned-repo");
    await page.locator("#clone-dest").fill(dest);
    await cloneBtn.click();

    await expect(page.getByText("Selected project")).toBeVisible({ timeout: 30_000 });
    await expect(page.getByText(dest, { exact: false })).toBeVisible();
    expect(existsSync(join(dest, ".git"))).toBe(true);
    await expect(launchButton(page)).toBeEnabled();
  });

  test("bare clone: creates worktree structure and returns main path", async ({ page, serve }) => {
    // A bare clone checks out a worktree, so the source needs a commit.
    const bare = createSeededBareRepo(serve.home, serve.env);
    await openCloneTab(page, serve);
    await page.locator("#clone-url").fill(bare.url);
    await page.getByRole("button", { name: /Advanced/ }).click();
    const dest = join(serve.home, "bare-clone-test");
    await page.locator("#clone-dest").fill(dest);
    await page.getByText("Clone as bare repository").click();
    // Shallow is incompatible with bare.
    await expect(page.locator('input[type="checkbox"]').first()).toBeDisabled();
    await page.getByRole("button", { name: "Clone repository" }).click();

    await expect(page.getByText("Selected project")).toBeVisible({ timeout: 30_000 });
    const mainPath = join(dest, "main");
    await expect(page.getByText(mainPath, { exact: false })).toBeVisible();
    for (const path of [join(dest, ".bare"), join(dest, ".git"), mainPath, join(mainPath, ".git")]) {
      expect(existsSync(path)).toBe(true);
    }
    await expect(launchButton(page)).toBeEnabled();
  });

  test("clone failure path: unrecognised URL scheme surfaces a server error", async ({ page, serve }) => {
    await openCloneTab(page, serve);
    await page.locator("#clone-url").fill("not-a-url");
    await page.getByRole("button", { name: "Clone repository" }).click();
    await expect(page.getByText("URL does not look like a git repository URL")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText("Selected project")).toHaveCount(0);
    await expect(launchButton(page)).toBeDisabled();
  });
});

test("diff base defaults to the worktree's base branch, override still wins", async ({ spawnServe }) => {
  // #1951. Auto-detection would pick `main`, so `release` proves the worktree base is used.
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      const primary = seedRepo(env, home, "primary", ["file.txt"]);
      run(env, ["branch", "release"], primary);
      writeFileSync(join(primary, "file2.txt"), "world\n");
      run(env, ["add", "file2.txt"], primary);
      run(env, ["commit", "-q", "-m", "commit B"], primary);
    },
  });
  const { id } = await createSession(serve, {
    path: join(serve.home, "primary"),
    tool: "claude",
    title: "diff-base-from-worktree",
    worktree_branch: "feature/diff-base-from-worktree",
    create_new_branch: true,
    base_branch: "release",
  });
  const bases = async () => {
    const res = await fetch(`${serve.baseUrl}/api/sessions/${id}/diff/files`);
    if (!res.ok) throw new Error(`GET diff/files failed: ${res.status} ${await res.text()}`);
    return ((await res.json()) as { per_repo_bases: { base_branch: string }[] }).per_repo_bases;
  };
  const setOverride = async (baseBranch: string) => {
    const res = await fetch(`${serve.baseUrl}/api/sessions/${id}/diff-base`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ base_branch: baseBranch }),
    });
    if (!res.ok) throw new Error(`PATCH diff-base failed: ${res.status} ${await res.text()}`);
  };

  const initial = await bases();
  expect(initial.length).toBe(1);
  expect(initial[0]!.base_branch).toBe("release");
  await setOverride("main");
  expect((await bases())[0]!.base_branch).toBe("main");
  await setOverride("");
  expect((await bases())[0]!.base_branch).toBe("release");
});

// #1511: branch off the canonical remote's base, not a stale fork tip, per repo.
test.describe("stale base on fork + upstream layouts", () => {
  /** `<name>` clones a fork whose branch lags `upstream` by one commit. */
  function seedForkUpstreamLayout(env: NodeJS.ProcessEnv, root: string, name: string, branch: string) {
    const upstreamDir = join(root, `${name}-upstream`);
    const originDir = join(root, `${name}-origin`);
    const localDir = join(root, name);
    mkdirSync(upstreamDir, { recursive: true });
    mkdirSync(originDir, { recursive: true });
    run(env, ["init", "--bare", "-q", `--initial-branch=${branch}`, upstreamDir], root);
    run(env, ["init", "--bare", "-q", `--initial-branch=${branch}`, originDir], root);

    const seed = join(root, `${name}-seed-a`);
    run(env, ["clone", "-q", upstreamDir, seed], root);
    writeFileSync(join(seed, "file.txt"), "hello\n");
    run(env, ["add", "file.txt"], seed);
    run(env, ["commit", "-q", "-m", "commit A"], seed, {
      GIT_AUTHOR_DATE: "1700000000 +0000",
      GIT_COMMITTER_DATE: "1700000000 +0000",
    });
    run(env, ["push", "-q", "origin", `HEAD:${branch}`], seed);
    run(env, ["remote", "add", "fork-origin", originDir], seed);
    run(env, ["push", "-q", "fork-origin", `HEAD:${branch}`], seed);
    writeFileSync(join(seed, "file2.txt"), "world\n");
    run(env, ["add", "file2.txt"], seed);
    run(env, ["commit", "-q", "-m", "commit B"], seed, {
      GIT_AUTHOR_DATE: "1700001000 +0000",
      GIT_COMMITTER_DATE: "1700001000 +0000",
    });
    run(env, ["push", "-q", "origin", `HEAD:${branch}`], seed);
    expect(run(env, ["rev-parse", branch], seed)).not.toBe(run(env, ["rev-parse", `fork-origin/${branch}`], seed));

    run(env, ["clone", "-q", originDir, localDir], root);
    run(env, ["remote", "add", "upstream", upstreamDir], localDir);
    run(env, ["fetch", "-q", "upstream"], localDir);
  }

  /** The workspace dir name embeds an internal id, so find it by prefix. */
  function workspaceRepo(home: string, branchSlug: string, repo: string) {
    const matches = readdirSync(home).filter((name) => name.startsWith(`${branchSlug}-workspace-`));
    if (matches.length !== 1) {
      throw new Error(
        `expected exactly one ${branchSlug}-workspace-* entry in ${home}, got: ${JSON.stringify(matches)}`,
      );
    }
    return join(home, matches[0]!, repo);
  }

  const head = (serve: ServeHandle, dir: string) => run(serve.env, ["rev-parse", "HEAD"], dir);
  const upstreamMain = (serve: ServeHandle, repo: string) =>
    run(serve.env, ["rev-parse", "upstream/main"], join(serve.home, repo));

  test("single-repo: explicit base_branch branches off fresh upstream tip, not stale origin", async ({
    spawnServe,
  }) => {
    const serve = await spawnServe({ seedFn: ({ home, env }) => seedForkUpstreamLayout(env, home, "primary", "main") });
    const created = await createSession(serve, {
      path: join(serve.home, "primary"),
      tool: "claude",
      title: "stale-base-single",
      worktree_branch: "feature/stale-base-single",
      create_new_branch: true,
      base_branch: "main",
    });
    expect(created.warnings ?? []).toEqual([]);
    expect(head(serve, join(serve.home, "primary-worktrees", "feature-stale-base-single"))).toBe(
      upstreamMain(serve, "primary"),
    );
  });

  test("multi-repo: secondary repo with fork+upstream layout branches off upstream tip", async ({ spawnServe }) => {
    const serve = await spawnServe({
      seedFn: ({ home, env }) => {
        seedForkUpstreamLayout(env, home, "primary", "main");
        seedForkUpstreamLayout(env, home, "secondary", "main");
      },
    });
    const created = await createSession(serve, {
      path: join(serve.home, "primary"),
      tool: "claude",
      title: "stale-base-multi",
      worktree_branch: "feature/stale-base-multi",
      create_new_branch: true,
      base_branch: "main",
      extra_repo_paths: [join(serve.home, "secondary")],
    });
    expect(created.warnings ?? []).toEqual([]);
    expect(created.workspace_repos).toBeDefined();
    expect((created.workspace_repos ?? []).length).toBeGreaterThanOrEqual(2);
    for (const repo of ["primary", "secondary"]) {
      expect(head(serve, workspaceRepo(serve.home, "feature-stale-base-multi", repo))).toBe(upstreamMain(serve, repo));
    }
  });

  test("multi-repo: per-repo fetch failure surfaces as warning, session still created", async ({ spawnServe }) => {
    const serve = await spawnServe({
      seedFn: ({ home, env }) => {
        seedRepo(env, home, "primary", ["file.txt"]);
        const secondary = seedRepo(env, home, "secondary", ["file.txt"]);
        run(env, ["remote", "add", "origin", join(home, "does-not-exist.git")], secondary);
      },
    });
    const secondary = join(serve.home, "secondary");
    const created = await createSession(serve, {
      path: join(serve.home, "primary"),
      tool: "claude",
      title: "fetch-fail",
      worktree_branch: "feature/fetch-fail",
      create_new_branch: true,
      extra_repo_paths: [secondary],
    });
    // User-facing toast text from record_fetch_warning in src/git/worktree.rs.
    const warnings = created.warnings ?? [];
    const pattern = new RegExp(
      `^git fetch \\S+ \\S+ failed for ${secondary.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}: .+`,
    );
    expect(
      warnings.find((w) => pattern.test(w)),
      `expected warning matching ${pattern} for ${secondary}, got: ${JSON.stringify(warnings)}`,
    ).toBeDefined();
    for (const repo of ["primary", "secondary"]) {
      expect(head(serve, workspaceRepo(serve.home, "feature-fetch-fail", repo))).toBeTruthy();
    }
  });
});
