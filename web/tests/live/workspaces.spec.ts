// Workspace and project structure against a real server: ordering, project pins, attaching repos.

import { spawnSync } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary } from "../helpers/aoeServe";
import { gitEnv, initWorkingRepo } from "../helpers/gitFixture";
import { readVisibleSessionTitles, seedSessionsInRepo } from "../helpers/sidebar";

function aoe(env: NodeJS.ProcessEnv, args: string[]) {
  const res = spawnSync(resolveAoeBinary(), args, { env });
  if (res.status !== 0) {
    throw new Error(`aoe ${args[0]} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
  }
}

test("press-and-hold drag reorders and round-trips PUT /api/workspace-ordering", async ({ page, spawnServe }) => {
  // #1220. Sessions without a branch have workspace id `<project_path>::__session__::<id>`.
  const serve = await spawnServe({ seedFn: seedSessionsInRepo({ titles: ["alpha", "beta", "gamma"] }) });
  const seeded = await listSessions(serve.baseUrl);
  expect(seeded).toHaveLength(3);
  const puts: string[][] = [];
  await page.route("**/api/workspace-ordering", (route) => {
    if (route.request().method() === "PUT") {
      const body = route.request().postDataJSON() as { order?: string[] };
      if (Array.isArray(body.order)) puts.push(body.order);
    }
    return route.continue();
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto(`${serve.baseUrl}/`);
  await expect
    .poll(() => readVisibleSessionTitles(page), { timeout: 8_000 })
    .toEqual(expect.arrayContaining(["alpha", "beta", "gamma"]));
  const initial = await readVisibleSessionTitles(page);
  expect(initial).toHaveLength(3);
  expect(new Set(initial)).toEqual(new Set(["alpha", "beta", "gamma"]));

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);
  const sourceBox = await wrappers.nth(2).boundingBox();
  const targetBox = await wrappers.nth(0).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row boxes missing");
  // Hold past dnd-kit's 150ms activation delay without moving more than 8px.
  await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });
  expect((await wrappers.nth(2).getAttribute("class")) ?? "").toContain("ring-2");
  await page.mouse.up();

  await expect
    .poll(() => readVisibleSessionTitles(page), { timeout: 4_000 })
    .toEqual([initial[2], initial[0], initial[1]]);
  const byTitle = new Map(
    seeded.map((s) => [s.title, `${(s.project_path as string).replace(/\/+$/, "")}::__session__::${s.id}`]),
  );
  await expect
    .poll(() => puts.at(-1), { timeout: 4_000 })
    .toEqual([byTitle.get(initial[2]!), byTitle.get(initial[0]!), byTitle.get(initial[1]!)]);
  const body = (await fetch(`${serve.baseUrl}/api/sessions`).then((r) => r.json())) as { workspace_ordering: string[] };
  expect(body.workspace_ordering.slice(0, 3)).toEqual(puts.at(-1));
});

test("saved-unpinned hidden; pin POSTs; unpin keeps the saved project", async ({ page, spawnServe }) => {
  // #2047, #2208: saving a project no longer adds a sidebar header, and unpinning must not delete it.
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      const projectA = initWorkingRepo(join(home, "projectA"), env).path;
      const projectB = initWorkingRepo(join(home, "projectB"), env).path;
      aoe(env, ["add", projectA, "-t", "pin-session", "-c", "claude"]);
      aoe(env, ["project", "add", projectB, "--scope", "global"]);
    },
  });
  const sessions = await listSessions(serve.baseUrl);
  expect(sessions).toHaveLength(1);
  const repoA = sessions[0]!.project_path as string;
  const savedProjects = async () =>
    (await (await page.request.get(`${serve.baseUrl}/api/projects`)).json()) as { name: string; path: string }[];

  await page.goto(`${serve.baseUrl}/`);
  const headerA = page.locator(`[data-testid='sidebar-group-header'][data-group-id='${repoA}']`);
  const marker = headerA.locator("[data-testid='sidebar-group-pinned-marker']");
  await expect(headerA).toBeVisible({ timeout: 10_000 });
  await expect(page.locator("[data-testid='sidebar-group-header']").filter({ hasText: "projectB" })).toHaveCount(0);
  expect((await savedProjects()).some((p) => p.name === "projectB")).toBe(true);

  await headerA.click({ button: "right" });
  const pinPost = page.waitForResponse(
    (res) => res.url().endsWith("/api/projects") && res.request().method() === "POST",
  );
  await page.locator("[data-testid='sidebar-group-context-menu-pin']").click();
  const pinRes = await pinPost;
  expect(pinRes.ok()).toBe(true);
  expect(pinRes.request().postDataJSON()).toMatchObject({ path: repoA, scope: "global", pinned: true });
  await expect(marker).toBeVisible({ timeout: 5_000 });

  await page.reload();
  await expect(marker).toBeVisible({ timeout: 10_000 });
  await headerA.click({ button: "right" });
  const unpinPatch = page.waitForResponse(
    (res) => res.url().includes("/api/projects/") && res.request().method() === "PATCH",
  );
  await page.locator("[data-testid='sidebar-group-context-menu-unpin']").click();
  const unpinRes = await unpinPatch;
  expect(unpinRes.ok()).toBe(true);
  expect(unpinRes.request().postDataJSON()).toMatchObject({ pinned: false });
  await expect(marker).toHaveCount(0, { timeout: 5_000 });
  expect((await savedProjects()).some((p) => p.path === repoA)).toBe(true);
});

test("attaching a project over the daemon converts the session into a workspace", async ({ spawnServe }) => {
  // #3103. No agent runs, so the worker outcome is deterministic.
  const git = (env: NodeJS.ProcessEnv, args: string[], cwd: string) => {
    const res = spawnSync("git", args, { cwd, env: gitEnv(env), encoding: "utf8" });
    if (res.error || res.status !== 0) {
      throw new Error(`git ${args.join(" ")} failed in ${cwd}: ${res.error ?? "non-zero exit"}; stderr=${res.stderr}`);
    }
    return res.stdout.trim();
  };
  const seedRepo = (env: NodeJS.ProcessEnv, home: string, name: string, extraBranch?: string) => {
    const dir = join(home, name);
    git(env, ["init", "-q", "--initial-branch=main", dir], home);
    writeFileSync(join(dir, "file.txt"), `${name}\n`);
    git(env, ["add", "file.txt"], dir);
    git(env, ["commit", "-q", "-m", "init"], dir);
    if (extraBranch) git(env, ["branch", extraBranch], dir);
  };
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      seedRepo(env, home, "backend");
      seedRepo(env, home, "frontend");
      // Already has the branch the session will use.
      seedRepo(env, home, "taken", "feature/attach-live");
    },
  });
  type AttachResponse = {
    attached: { name: string; branch: string; branch_created: boolean; moved_to: string | null };
    worker: string;
  };
  const attach = async (
    handle: ServeHandle,
    id: string,
    body: { project: string; attach_existing_branch?: boolean },
  ) => {
    const res = await fetch(`${handle.baseUrl}/api/sessions/${id}/projects`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    return { status: res.status, text: await res.text() };
  };
  const attachOk = async (...args: Parameters<typeof attach>) => {
    const { status, text } = await attach(...args);
    if (status !== 200) throw new Error(`POST projects failed: ${status} ${text}`);
    return JSON.parse(text) as AttachResponse;
  };

  const created = await fetch(`${serve.baseUrl}/api/sessions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      path: join(serve.home, "backend"),
      tool: "claude",
      title: "attach-live",
      worktree_branch: "feature/attach-live",
      create_new_branch: true,
    }),
  });
  if (!created.ok) throw new Error(`POST /api/sessions failed: ${created.status} ${await created.text()}`);
  const { id } = (await created.json()) as { id: string };

  const attached = await attachOk(serve, id, { project: join(serve.home, "frontend") });
  expect(attached.attached.name).toBe("frontend");
  expect(attached.attached.branch).toBe("feature/attach-live");
  expect(attached.attached.branch_created).toBe(true);
  expect(attached.worker).toBe("not_running");

  // The worktree session converts into a workspace directory holding both repos.
  const workspaceDir = attached.attached.moved_to;
  expect(workspaceDir).toBeTruthy();
  for (const name of ["backend", "frontend"]) {
    const worktree = join(workspaceDir!, name);
    expect(existsSync(join(worktree, ".git"))).toBe(true);
    expect(git(serve.env, ["rev-parse", "--abbrev-ref", "HEAD"], worktree)).toBe("feature/attach-live");
  }
  const appDir = appDirFor(serve.home, serve.env.XDG_CONFIG_HOME!, resolveAoeBinary());
  expect(existsSync(join(appDir, "attached-repos"))).toBe(false);

  const widened = (await listSessions(serve.baseUrl)).find((s) => s.id === id) as unknown as {
    workspace_repos: { name: string }[];
    project_path: string;
  };
  expect(widened.workspace_repos.map((r) => r.name)).toEqual(["backend", "frontend"]);
  expect(widened.project_path).toBe(workspaceDir);

  const dupe = await attach(serve, id, { project: join(serve.home, "frontend") });
  expect(dupe.status).toBe(400);
  expect(dupe.text).toContain("already attached");

  // An existing same-named branch can hold unrelated commits, so it needs an explicit opt-in.
  const takenPath = join(serve.home, "taken");
  const refused = await attach(serve, id, { project: takenPath });
  expect(refused.status).toBe(400);
  expect(refused.text).toContain("already exists");
  const reused = await attachOk(serve, id, { project: takenPath, attach_existing_branch: true });
  expect(reused.attached.name).toBe("taken");
  // Not created by aoe, so session deletion leaves it alone.
  expect(reused.attached.branch_created).toBe(false);
  expect(reused.attached.moved_to).toBeNull();
  expect(existsSync(join(workspaceDir!, "taken", ".git"))).toBe(true);

  // Neither a non-repo path nor an unregistered bare name is accepted.
  expect((await attach(serve, id, { project: join(serve.home, "nope") })).status).toBe(400);
  expect((await attach(serve, id, { project: "not-registered" })).status).toBe(400);
});
