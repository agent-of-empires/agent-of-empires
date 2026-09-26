// Settings panels backed by the real server: MCP servers, skills, plugins, and profile overrides.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect, authHeaders, bootDashboard, type ServeHandle } from "../helpers/liveTest";

test.describe("MCP servers", () => {
  test("MCP panel shows native servers with provenance and redacts secrets", async ({ page, spawnServe }) => {
    // #1996
    const serve = await spawnServe({
      seedFn: ({ home }) =>
        writeFileSync(
          join(home, ".claude.json"),
          JSON.stringify({
            mcpServers: {
              fs: { command: "mcp-fs", args: ["--root", "."], env: { TOKEN: "SUPER_SECRET_DO_NOT_LEAK" } },
              remote: {
                type: "http",
                url: "https://example/mcp",
                headers: { Authorization: "Bearer HEADER_SECRET_DO_NOT_LEAK" },
              },
            },
          }),
        ),
    });
    await page.goto(`${serve.baseUrl}/settings/mcp`);
    const panel = page.getByTestId("mcp-panel");
    await expect(panel).toBeVisible();
    await expect(panel.getByText("fs", { exact: true })).toBeVisible();
    await expect(panel.getByText("remote", { exact: true })).toBeVisible();
    await expect(panel.getByText("agent-native:claude").first()).toBeVisible();
    // Secret values never render; their names do.
    await expect(panel).not.toContainText("SUPER_SECRET_DO_NOT_LEAK");
    await expect(panel).not.toContainText("HEADER_SECRET_DO_NOT_LEAK");
    await expect(panel).toContainText("TOKEN");
    await expect(panel).toContainText("Authorization");
  });
});

test("skills panel adopts, edits, creates, and deletes skills", async ({ page, spawnServe }) => {
  // #3050
  const original = "---\nname: Review\ndescription: Review code carefully\n---\n\nOriginal body\n";
  const serve = await spawnServe({
    seedFn: ({ home }) => {
      const skill = join(home, ".claude", "skills", "review");
      mkdirSync(skill, { recursive: true });
      writeFileSync(join(skill, "SKILL.md"), original);
    },
  });
  // #3263: the update banner is also role=status, so force it on to keep status queries honest.
  await page.route("**/api/system/update-status", (route) =>
    route.fulfill({
      json: {
        update_check_mode: "notify",
        current_version: "0.0.1",
        latest_version: "99.0.0",
        update_available: true,
        release_url: null,
        error: null,
        dismissed_version: null,
      },
    }),
  );
  const notice = (text: string) => page.getByRole("status").filter({ hasText: text });
  const content = page.getByLabel("SKILL.md content");

  await page.goto(`${serve.baseUrl}/settings/skills`);
  await expect(page.getByRole("status", { name: /^Update available/ })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Skills Library" })).toBeVisible();
  // Anchored: the "Preview" toggle also contains "review".
  await page.getByRole("button", { name: /^review/i }).click();
  await expect(page.getByText("claude-user").first()).toBeVisible();
  await expect(content).toHaveAttribute("readonly", "");

  await page.getByRole("button", { name: "Adopt into AoE" }).click();
  await expect(notice("adopted")).toBeVisible();
  await expect(content).not.toHaveAttribute("readonly");
  expect(readFileSync(join(serve.home, ".claude", "skills", "review", "SKILL.md"), "utf8")).toBe(original);

  const edited = "---\nname: Review\ndescription: Updated review\n---\n\nEdited body\n";
  await content.fill(edited);
  await page.getByRole("button", { name: "Save" }).click();
  await expect(notice("saved")).toBeVisible();
  const persisted = await page.request.get(`${serve.baseUrl}/api/skills/aoe-managed/review`);
  expect(persisted.ok()).toBe(true);
  expect((await persisted.json()).content).toBe(edited);

  await page.getByRole("button", { name: "+ New skill" }).click();
  await page.getByLabel("New skill directory").fill("new-skill");
  await page.getByLabel("New skill description").fill("Use for new work");
  await page.getByRole("button", { name: "Create" }).click();
  await expect(notice("created")).toBeVisible();
  await expect(content).toHaveValue(/name: new-skill/);

  page.once("dialog", (dialog) => void dialog.accept());
  await page.getByRole("button", { name: "Delete" }).click();
  await expect(notice("deleted")).toBeVisible();
  expect((await page.request.get(`${serve.baseUrl}/api/skills/aoe-managed/new-skill`)).status()).toBe(404);
});

// The builtin aoe.web needs no install; disabling it at runtime does not stop the running server.
test.describe("plugins", () => {
  async function webEnabled(handle: ServeHandle): Promise<boolean> {
    const data: { plugins: { id: string; enabled: boolean }[] } = await fetch(`${handle.baseUrl}/api/plugins`, {
      headers: authHeaders(handle),
    }).then((r) => r.json());
    const web = data.plugins.find((p) => p.id === "aoe.web");
    expect(web, "aoe.web must be present in the live registry").toBeTruthy();
    return web!.enabled;
  }
  const webToggle = (page: Page) => page.getByLabel("Enable Web Dashboard");

  test("disabling a builtin plugin persists to the backend and survives a reload", async ({ serve, page }) => {
    // #268, #2090
    expect(await webEnabled(serve)).toBe(true);
    await page.goto(`${serve.baseUrl}/settings/plugins`);
    await expect(webToggle(page)).toBeVisible({ timeout: 10_000 });
    await expect(webToggle(page)).toBeChecked();
    // The checkbox only flips after the async round-trip, so click rather than uncheck.
    await webToggle(page).click();
    await expect(async () => {
      expect(await webEnabled(serve)).toBe(false);
    }).toPass({ timeout: 5_000 });
    await page.reload();
    await expect(webToggle(page)).not.toBeChecked({ timeout: 10_000 });
  });

  test("loopback plugin toggle needs no passphrase elevation", async ({ servePreauthed, page }) => {
    // #2610: loopback plugin mutations used to 403 and loop the passphrase prompt.
    expect(await webEnabled(servePreauthed)).toBe(true);
    await bootDashboard(page, servePreauthed, "/settings/plugins");
    await page.evaluate(() => {
      const w = window as unknown as { __elevationFired?: boolean };
      w.__elevationFired = false;
      window.addEventListener("aoe:elevation-required", () => {
        w.__elevationFired = true;
      });
    });
    await expect(webToggle(page)).toBeVisible({ timeout: 10_000 });
    await expect(webToggle(page)).toBeChecked();
    await webToggle(page).click();
    await expect(async () => {
      expect(await webEnabled(servePreauthed)).toBe(false);
    }).toPass({ timeout: 5_000 });
    await expect(page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i })).toHaveCount(0);
    expect(
      await page.evaluate(() => (window as unknown as { __elevationFired?: boolean }).__elevationFired ?? false),
    ).toBe(false);
  });
});

test("per-profile setting override leaves global state untouched", async ({ serve, page }) => {
  type Settings = { session?: { default_tool?: string | null } };
  const getSettings = (path: string): Promise<Settings> => fetch(`${serve.baseUrl}${path}`).then((r) => r.json());
  const createRes = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "work" }),
  });
  expect(createRes.ok).toBeTruthy();

  const sentinel = "claude-code-override-test";
  const globalBefore = await getSettings("/api/settings");
  expect(globalBefore?.session?.default_tool).not.toBe(sentinel);
  expect((await getSettings("/api/profiles/work/settings"))?.session?.default_tool).not.toBe(sentinel);

  const patches: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "PATCH" && req.url().includes("/api/")) patches.push(req.url());
  });
  await page.goto(`${serve.baseUrl}/settings/session`);
  await expect(page.getByTestId("settings-header").getByText("Profile", { exact: true })).toBeVisible();
  const profileSelect = page
    .locator("label", { hasText: /^Profile$/ })
    .locator("..")
    .locator("select");
  await profileSelect.selectOption("work");
  await expect(profileSelect).toHaveValue("work");

  // The TextField saves on blur.
  const defaultTool = page
    .locator("label", { hasText: /^Default Tool$/ })
    .locator("..")
    .locator("input[type=text]");
  await defaultTool.fill(sentinel);
  await defaultTool.blur();
  await expect(async () => {
    expect((await getSettings("/api/profiles/work/settings"))?.session?.default_tool).toBe(sentinel);
  }).toPass({ timeout: 5_000 });

  // The server omits unset fields, so compare as null.
  const globalAfter = await getSettings("/api/settings");
  expect(globalAfter?.session?.default_tool ?? null).toBe(globalBefore?.session?.default_tool ?? null);
  expect(globalAfter?.session?.default_tool ?? null).not.toBe(sentinel);
  expect(patches.some((url) => url.endsWith("/api/profiles/work/settings"))).toBe(true);
  expect(patches.some((url) => url.endsWith("/api/settings"))).toBe(false);
});
