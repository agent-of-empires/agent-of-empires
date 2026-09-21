// Creating sessions against a real server: the wizard, scratch sessions, the palette, directory browsing, worktrees.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import type { Locator, Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd, waitForSessions, waitForView } from "../helpers/aoeServe";
import { postPrompt, startAcpSession, waitForReplayContains, waitForStructuredView } from "../helpers/acp";
import { gitEnv, initWorkingRepo } from "../helpers/gitFixture";

const PALETTE_PLACEHOLDER = "Search actions, sessions, settings…";

async function openWizard(page: Page, serve: ServeHandle): Promise<Locator> {
  await page.goto(serve.baseUrl);
  await page.getByRole("button", { name: "New session", exact: true }).first().click();
  const wizard = page.locator('[data-testid="session-wizard"]');
  await expect(wizard).toBeVisible({ timeout: 15_000 });
  return wizard;
}

/** The only session is a scratch session under the app data dir's `scratch/`. */
async function expectOneScratchSession(serve: ServeHandle) {
  const sessions = await waitForSessions(serve.baseUrl);
  expect(sessions).toHaveLength(1);
  expect(sessions[0]!.scratch).toBe(true);
  expect(basename(dirname(sessions[0]!.project_path as string))).toBe("scratch");
  return sessions[0]!;
}

async function waitForReadOnlyDashboard(page: Page, serve: ServeHandle) {
  // Read-only guards read /api/about, so wait for it before interacting.
  const about = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, { timeout: 10_000 });
  await page.goto(serve.baseUrl);
  await about;
  await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();
}

test.describe("wizard", () => {
  test("wizard with Use structured view on creates a structured_view session", async ({ page, spawnServe }) => {
    // #1841: the structured view toggle defaults on for an ACP-capable agent.
    const serve = await spawnServe({ acp: true });
    const wizard = await openWizard(page, serve);
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "More options" }).click();
    const acpToggle = wizard.getByRole("switch", { name: "Use structured view" });
    await expect(acpToggle).toBeVisible({ timeout: 10_000 });
    await expect(acpToggle).toBeChecked();
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    const sessions = await waitForSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    await waitForView(serve.baseUrl, sessions[0]!.id, "structured");
  });

  test("wizard auto-approve starts Codex in full-access mode", async ({ page, spawnServe }) => {
    const serve = await spawnServe({ acp: true, extraEnv: { FAKE_ACP_MODE_VIA_CONFIG_OPTION: "codex" } });
    const wizard = await openWizard(page, serve);
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "codex", exact: true }).click();
    await wizard.getByRole("button", { name: "More options" }).click();
    const autoApprove = wizard.getByRole("switch", { name: "Auto-approve actions" });
    await autoApprove.click();
    await expect(autoApprove).toBeChecked();
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    await waitForStructuredView(page);
    await expect(page.getByRole("button", { name: /Agent \(full access\)/ }).first()).toBeVisible({ timeout: 15_000 });
    const sessions = await waitForSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    expect(sessions[0]!.tool).toBe("codex");
    expect(sessions[0]!.yolo_mode).toBe(true);
  });

  test("project stays in the wizard Recent tab after its last session is deleted (#2141)", async ({
    page,
    spawnServe,
  }) => {
    const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "frontend-work", subdir: "frontend" }) });
    const [seeded] = await listSessions(serve.baseUrl);
    const del = await page.request.delete(`${serve.baseUrl}/api/sessions/${seeded!.id}`, { data: {} });
    expect(del.ok()).toBe(true);
    await expect.poll(async () => (await listSessions(serve.baseUrl)).length, { timeout: 10_000 }).toBe(0);

    const recent = await (await page.request.get(`${serve.baseUrl}/api/recent-projects`)).json();
    expect((recent.projects as { display_name: string }[]).map((p) => p.display_name)).toContain("frontend");

    const wizard = await openWizard(page, serve);
    await expect(wizard.getByText("frontend", { exact: true })).toBeVisible({ timeout: 10_000 });
    await expect(wizard.getByText("0 sessions")).toBeVisible();
  });
});

// #1324
test.describe("scratch sessions", () => {
  test("scratch happy path: launch creates a scratch-dir session", async ({ page, spawnServe }) => {
    const serve = await spawnServe();
    const wizard = await openWizard(page, serve);
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await expect(wizard.getByText("Scratch session")).toBeVisible({ timeout: 10_000 });
    await wizard.getByRole("button", { name: /Launch session/ }).click();
    const session = await expectOneScratchSession(serve);

    // Creation is durable across a fresh server load.
    await serve.restart();
    const reloaded = await listSessions(serve.baseUrl);
    expect(reloaded.map((row) => row.id)).toEqual([session.id]);
    expect(reloaded[0]!.scratch).toBe(true);
    expect(reloaded[0]!.project_path).toBe(session.project_path);
  });

  test("deleting a scratch session removes its scratch dir", async ({ page, spawnServe }) => {
    const serve = await spawnServe();
    const wizard = await openWizard(page, serve);
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: /Launch session/ }).click();
    const [created] = await waitForSessions(serve.baseUrl);
    const projectPath = created!.project_path as string;
    expect(existsSync(projectPath)).toBe(true);

    const row = page.locator("[data-testid='sidebar-session-row']").first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.click({ button: "right" });
    await page.locator("[data-testid='sidebar-context-menu-delete']").click();
    const dialog = page.locator("[data-testid='delete-session-dialog']");
    await expect(dialog).toBeVisible();
    // Trash is the default; only a permanent delete purges the directory.
    await dialog.locator("[data-testid='delete-session-permanent']").click();
    const deletePromise = page.waitForResponse(
      (res) => res.url().endsWith(`/api/workspaces`) && res.request().method() === "DELETE",
    );
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    const deleteRes = await deletePromise;
    expect(deleteRes.ok()).toBe(true);
    expect((deleteRes.request().postDataJSON() as { session_ids: string[] }).session_ids).toEqual([created!.id]);

    await expect.poll(async () => (await listSessions(serve.baseUrl)).length, { timeout: 10_000 }).toBe(0);
    await expect.poll(() => existsSync(projectPath), { timeout: 5_000 }).toBe(false);
  });

  test("palette 'New scratch session' opens the wizard and launches a scratch session", async ({ serve, page }) => {
    // #1643
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "New session", exact: true }).first()).toBeVisible({
      timeout: 15_000,
    });
    await page.keyboard.press("ControlOrMeta+KeyK");
    await expect(page.getByPlaceholder(PALETTE_PLACEHOLDER)).toBeVisible();
    await page.getByPlaceholder(PALETTE_PLACEHOLDER).fill("scratch");
    await page.getByRole("option", { name: /New scratch session/i }).click();

    const wizard = page.locator('[data-testid="session-wizard"]');
    await expect(wizard).toBeVisible({ timeout: 10_000 });
    await expect(wizard.getByRole("button", { name: /Launch session/ })).toBeVisible({ timeout: 10_000 });
    await expect(wizard.getByText("Scratch session")).toBeVisible();
    await page.keyboard.press("ControlOrMeta+Enter");
    await expectOneScratchSession(serve);
  });

  test("palette hides creation commands in read-only mode", async ({ serveReadOnly, page }) => {
    await waitForReadOnlyDashboard(page, serveReadOnly);
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+KeyK");
    await expect(page.getByPlaceholder(PALETTE_PLACEHOLDER)).toBeVisible();
    await expect(page.getByRole("option", { name: /New scratch session/i })).toHaveCount(0);
    // #2108: the "New Session Mode" setting entry also matches but opens settings.
    await expect(
      page.getByRole("option", { name: /New session/i }).filter({ hasNotText: "Opens settings" }),
    ).toHaveCount(0);
  });
});

test("search surfaces a session by its conversation content", async ({ spawnServe }) => {
  // #2515: the token appears only in the prompt, so a hit proves content search.
  const needle = "xyzzycontentneedle";
  const { serve, sessionId } = await startAcpSession(spawnServe, { title: "content-search" });
  const promptRes = await postPrompt(serve.baseUrl, sessionId, `please remember ${needle} for later`);
  expect(promptRes.status).toBeGreaterThanOrEqual(200);
  expect(promptRes.status).toBeLessThan(300);
  await waitForReplayContains(serve.baseUrl, sessionId, ["user_prompt_sent", "UserPromptSent"]);

  const search = async (q: string) => {
    const res = await fetch(`${serve.baseUrl}/api/sessions/search?q=${q}`);
    return res.ok ? ((await res.json()).results ?? []).map((h: { session_id: string }) => h.session_id) : [];
  };
  await expect.poll(() => search(needle), { timeout: 10_000 }).toContain(sessionId);
  expect(await search("zzznevertypedthis")).toHaveLength(0);
});

// #1219: the browser persists the picked repo's parent so reopening shows its siblings.
test.describe("directory browser", () => {
  const option = (page: Page, text: string) => page.getByRole("option").filter({ hasText: text });
  async function openWizardWithShortcut(page: Page) {
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible({ timeout: 10_000 });
  }

  test("DirectoryBrowser: home -> projects -> repo-a -> persisted last dir", async ({ page, spawnServe }) => {
    const serve = await spawnServe({
      seedFn: ({ home }) => {
        const repoA = join(home, "projects", "repo-a");
        mkdirSync(join(repoA, ".git"), { recursive: true });
        writeFileSync(join(repoA, ".git", "HEAD"), "ref: refs/heads/main\n");
        mkdirSync(join(home, "projects", "repo-b"), { recursive: true });
        mkdirSync(join(home, ".hidden-proj"), { recursive: true });
      },
    });
    await page.goto(serve.baseUrl);
    const homeRes = await fetch(`${serve.baseUrl}/api/filesystem/home`);
    expect(homeRes.ok).toBeTruthy();
    const { path: homePath } = (await homeRes.json()) as { path: string };
    expect(homePath).toBeTruthy();

    // With no recent sessions the wizard opens on Browse.
    await openWizardWithShortcut(page);
    await expect(option(page, "projects")).toBeVisible({ timeout: 10_000 });
    // #3430: hidden folders are filtered server-side until requested.
    const hiddenToggle = page.getByRole("checkbox", { name: "Show hidden folders" });
    await expect(option(page, ".hidden-proj")).toHaveCount(0);
    await hiddenToggle.check();
    await expect(option(page, ".hidden-proj")).toBeVisible({ timeout: 10_000 });
    await hiddenToggle.uncheck();
    await expect(option(page, ".hidden-proj")).toHaveCount(0);

    await option(page, "projects").click();
    await expect(option(page, "repo-a")).toBeVisible();
    await expect(option(page, "repo-b")).toBeVisible();
    // Only repo-a has .git, which adds the "repo" badge to its name.
    await expect(option(page, "repo-a")).toHaveAccessibleName(/^repo-a\s+repo$/);
    await expect(option(page, "repo-b")).toHaveAccessibleName(/^repo-b$/);

    await option(page, "repo-a").click();
    await expect(page.getByText("Selected project")).toBeVisible();
    await expect(page.getByText(`${homePath}/projects/repo-a`)).toBeVisible();
    expect(await page.evaluate(() => window.localStorage.getItem("aoe-last-browse-dir"))).toBe(`${homePath}/projects`);

    await page.getByRole("button", { name: "Close" }).click();
    await openWizardWithShortcut(page);
    await expect(option(page, "repo-a")).toBeVisible({ timeout: 10_000 });
    await expect(option(page, "projects")).toHaveCount(0);
  });

  test("DirectoryBrowser: parent-dir row navigates up one level", async ({ page, spawnServe }) => {
    const serve = await spawnServe({
      seedFn: ({ home }) => void mkdirSync(join(home, "projects", "nested"), { recursive: true }),
    });
    await page.goto(serve.baseUrl);
    await openWizardWithShortcut(page);
    await option(page, "projects").click({ timeout: 10_000 });
    await option(page, "nested").click();
    await expect(page.getByText("No visible subfolders here")).toBeVisible();
    await option(page, "(parent directory)").click();
    await expect(option(page, "nested")).toBeVisible({ timeout: 5_000 });
  });
});

test.describe("worktrees", () => {
  const CIVILIZATION_NAMES = (
    "Armenians Aztecs Bengalis Berbers Bohemians Britons Bulgarians Burgundians Burmese Byzantines Celts Chinese " +
    "Cumans Dravidians Ethiopians Franks Georgians Goths Gurjaras Hindustanis Huns Incas Italians Japanese Jurchens " +
    "Khitans Khmer Koreans Lithuanians Magyars Malay Malians Mayans Mongols Persians Poles Portuguese Romans " +
    "Saracens Shu Sicilians Slavs Spanish Tatars Teutons Turks Vietnamese Vikings Wei Wu"
  ).split(" ");
  const createSession = (baseUrl: string, body: object) =>
    fetch(`${baseUrl}/api/sessions`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });

  test("duplicate worktree branch returns the real collision error, not a generic one", async ({ spawnServe }) => {
    // #1649
    const serve = await spawnServe({ seedFn: ({ home, env }) => void initWorkingRepo(join(home, "project"), env) });
    const payload = {
      path: join(serve.home, "project"),
      tool: "claude",
      title: "dup-session",
      worktree_branch: "dup-branch",
      create_new_branch: true,
    };
    expect((await createSession(serve.baseUrl, payload)).status).toBe(201);
    const second = await createSession(serve.baseUrl, payload);
    expect(second.status).toBe(400);
    const body = await second.json();
    expect(body.message).toContain("Worktree already exists");
    expect(body.message).not.toBe("Failed to create session");
  });

  test("auto-generated worktree branch avoids civilization branch collisions", async ({ spawnServe }) => {
    const serve = await spawnServe({
      seedFn: ({ home, env }) => {
        const { path } = initWorkingRepo(join(home, "project"), env);
        for (const civ of CIVILIZATION_NAMES) spawnSync("git", ["branch", civ], { cwd: path, env: gitEnv(env) });
      },
    });
    const res = await createSession(serve.baseUrl, {
      path: join(serve.home, "project"),
      tool: "claude",
      worktree_enabled: true,
      create_new_branch: true,
    });
    expect(res.status).toBe(201);
    const body = await res.json();
    expect(body.title).toMatch(/\bII\b/);
    expect(body.branch).toBeTruthy();
    expect(CIVILIZATION_NAMES.map((civ) => civ.toLowerCase())).not.toContain(String(body.branch).toLowerCase());
  });
});
