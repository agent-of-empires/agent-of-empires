// Settings persist through the real server: REST round-trips, the schema UI, and the theme picker.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Locator, Page } from "@playwright/test";
import { test, expect, authHeaders, bootDashboard, type ServeHandle } from "../helpers/liveTest";
import { appDirFor, loginWithPassphrase, resolveAoeBinary, spawnAoeServe } from "../helpers/aoeServe";

// #1189: the theme resolver once deadlocked per request, which mocked specs cannot see.
test.describe("theme API", () => {
  let handle: ServeHandle;
  test.beforeAll(async ({}, workerInfo) => {
    handle = await spawnAoeServe({ workerIndex: workerInfo.workerIndex, parallelIndex: workerInfo.parallelIndex });
  });
  test.afterAll(async () => {
    await handle?.stop();
  });

  const fetchTheme = async (name: string) => {
    const res = await fetch(`${handle.baseUrl}/api/themes/${name}`, { signal: AbortSignal.timeout(2_000) });
    expect(res.ok, `${name} did not return`).toBe(true);
    return res.json();
  };

  test("GET /api/themes/:name handles all 6 builtins sequentially without hanging", async () => {
    for (const name of ["empire", "phosphor", "tokyo-night-storm", "catppuccin-latte", "dracula", "rose-pine"]) {
      const body = await fetchTheme(name);
      expect(body.name).toBe(name);
      expect(body.web.cssVars).toBeTruthy();
    }
  });
});

const DRACULA_SURFACE = "#282a36";
const labelledSelect = (page: Page, label: RegExp) =>
  page.locator("label", { hasText: label }).locator("..").locator("select");
const getJson = (url: string, handle?: ServeHandle) =>
  fetch(url, { headers: handle ? authHeaders(handle) : {} }).then((r) => r.json());
const readInPage = (page: Page, url: string) => page.evaluate(async (u) => (await fetch(u)).json(), url);
const confirmPassphraseDialog = (page: Page) =>
  page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i });

async function defaultProfile(handle: ServeHandle): Promise<string> {
  const profiles: Array<{ name: string; is_default?: boolean }> = await getJson(
    `${handle.baseUrl}/api/profiles`,
    handle,
  );
  return profiles.find((p) => p.is_default)?.name ?? profiles[0]?.name ?? "main";
}

async function pickDracula(select: Locator) {
  await expect(select).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(
      () => select.evaluate((sel: HTMLSelectElement) => Array.from(sel.options).some((o) => o.value === "dracula")),
      { timeout: 5_000 },
    )
    .toBe(true);
  await select.selectOption("dracula");
}

async function expectRepaint(page: Page) {
  await expect
    .poll(() => page.evaluate(() => document.documentElement.style.getPropertyValue("--color-surface-900").trim()), {
      timeout: 5_000,
      intervals: [100, 200, 400],
    })
    .toBe(DRACULA_SURFACE);
}

test("structured view settings persist through PATCH + reload, node_path is stripped", async ({ serve, page }) => {
  // #1689: the section was missing from the allowlist. node_path is a local-only RCE surface.
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const baselineAcp = ((await getJson(settingsUrl))?.acp ?? {}) as Record<string, unknown>;
  const baselineNodePath = typeof baselineAcp.node_path === "string" ? baselineAcp.node_path : "";
  const newIdle = baselineAcp.auto_stop_idle_secs === 28800 ? 14400 : 28800;

  const patchRes = await fetch(settingsUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ acp: { ...baselineAcp, auto_stop_idle_secs: newIdle, node_path: "/tmp/evil-node" } }),
  });
  expect(patchRes.ok).toBeTruthy();
  const after = await getJson(settingsUrl);
  expect(after?.acp?.auto_stop_idle_secs).toBe(newIdle);
  expect(after?.acp?.node_path).toBe(baselineNodePath);
  expect(after?.acp?.node_path).not.toBe("/tmp/evil-node");

  await page.goto(serve.baseUrl);
  const fetched = await readInPage(page, settingsUrl);
  expect(fetched?.acp?.auto_stop_idle_secs).toBe(newIdle);
  expect(fetched?.acp?.node_path).not.toBe("/tmp/evil-node");
});

test("theme picker repaints, persists across reload and serve restart (#1510)", async ({ serve, page }) => {
  // The picker writes the global /api/theme endpoint and must dispatch its repaint only after the write.
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  await page.goto(`${serve.baseUrl}/settings/theme`);
  await pickDracula(labelledSelect(page, /^Theme$/));
  await expect(async () => {
    expect((await getJson(settingsUrl))?.theme?.name).toBe("dracula");
  }).toPass({ timeout: 5_000 });
  await expectRepaint(page);
  await expect(confirmPassphraseDialog(page)).toHaveCount(0);

  await page.reload();
  expect((await readInPage(page, settingsUrl))?.theme?.name).toBe("dracula");
  await serve.restart();
  expect((await getJson(settingsUrl))?.theme?.name).toBe("dracula");
});

// #1510: a passphrase session is authenticated but not elevated.
test.describe("passphrase mode", () => {
  test("theme picker persists across reload + restart without passphrase prompt", async ({ servePreauthed, page }) => {
    const settingsUrl = `${servePreauthed.baseUrl}/api/settings`;
    await bootDashboard(page, servePreauthed, "/settings/theme");
    await page.evaluate(() => {
      const w = window as unknown as { __elevationFired?: boolean };
      w.__elevationFired = false;
      window.addEventListener("aoe:elevation-required", () => {
        w.__elevationFired = true;
      });
    });

    await pickDracula(labelledSelect(page, /^Theme$/));
    await expect(async () => {
      expect((await getJson(settingsUrl, servePreauthed))?.theme?.name).toBe("dracula");
    }).toPass({ timeout: 5_000 });
    await expect(confirmPassphraseDialog(page)).toHaveCount(0);
    expect(
      await page.evaluate(() => (window as unknown as { __elevationFired?: boolean }).__elevationFired ?? false),
    ).toBe(false);
    await expectRepaint(page);

    await Promise.all([
      page.waitForResponse((res) => res.url().endsWith("/api/about") && res.status() === 200, { timeout: 10_000 }),
      page.reload(),
    ]);
    expect((await getJson(settingsUrl, servePreauthed))?.theme?.name).toBe("dracula");

    await servePreauthed.restart();
    const { cookie } = await loginWithPassphrase(
      servePreauthed.baseUrl,
      servePreauthed.passphrase!,
      servePreauthed.deviceBindingSecret!,
    );
    servePreauthed.sessionCookie = cookie;
    expect((await getJson(settingsUrl, servePreauthed))?.theme?.name).toBe("dracula");
  });

  test("sandbox image change requires elevation for remote callers, not loopback", async ({ servePreauthed, page }) => {
    const profile = await defaultProfile(servePreauthed);
    await bootDashboard(page, servePreauthed);
    // From the page, so the SPA's interceptor sees a 403 and opens the prompt. A TEST-NET-3
    // X-Forwarded-For from a loopback socket resolves as a remote caller.
    const patchImage = (image: string, xff?: string) =>
      page.evaluate(
        async ({ profile, image, xff }) =>
          (
            await fetch(`/api/profiles/${encodeURIComponent(profile)}/settings`, {
              method: "PATCH",
              headers: { "Content-Type": "application/json", ...(xff ? { "X-Forwarded-For": xff } : {}) },
              body: JSON.stringify({ sandbox: { default_image: image } }),
            })
          ).status,
        { profile, image, xff },
      );
    const savedImage = async () =>
      (await getJson(`${servePreauthed.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`, servePreauthed))
        ?.sandbox?.default_image;

    // #2610: loopback is trusted and must not loop the prompt.
    expect(await patchImage("ghcr.io/example/img:local-trusted")).toBe(200);
    await expect(confirmPassphraseDialog(page)).toHaveCount(0);

    expect(await patchImage("ghcr.io/example/img:tampered", "203.0.113.10")).toBe(403);
    const dialog = confirmPassphraseDialog(page);
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    expect((await savedImage()) ?? "").not.toBe("ghcr.io/example/img:tampered");

    // Elevation is per session, so the remote retry succeeds.
    await dialog.locator('input[type="password"]').fill(servePreauthed.passphrase!);
    await dialog.getByRole("button", { name: /Confirm/i }).click();
    await expect(dialog).toHaveCount(0, { timeout: 5_000 });
    expect(await patchImage("ghcr.io/example/img:elevated", "203.0.113.10")).toBe(200);
    expect(await savedImage()).toBe("ghcr.io/example/img:elevated");
  });
});

test("global settings migrate from profiles and reject stale profile writes", async ({ spawnServe, page }) => {
  let appDir = "";
  const serve = await spawnServe({
    seedFn: ({ home, xdg }) => {
      appDir = appDirFor(home, xdg, resolveAoeBinary());
      writeFileSync(join(appDir, ".schema_version"), "29");
      writeFileSync(join(appDir, "config.toml"), "default_profile = 'work'\n[theme]\nname = 'empire'\n");
      for (const name of ["alpha", "work"]) mkdirSync(join(appDir, "profiles", name), { recursive: true });
      writeFileSync(join(appDir, "profiles", "alpha", "config.toml"), "[theme]\nname = 'rose-pine'\n");
      writeFileSync(
        join(appDir, "profiles", "work", "config.toml"),
        "[theme]\nname = 'dracula'\nidle_decay_minutes = 5\n[session]\nsidebar_position = 'left'\nconfirm_before_quit = false\nsession_id_poller_max_threads = 12\ndefault_tool = 'codex'\n[web]\nnotify_on_idle = true\n",
      );
    },
  });
  const profile = await defaultProfile(serve);
  const globalUrl = `${serve.baseUrl}/api/settings`;
  const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`;
  const effectiveUrl = `${globalUrl}?profile=${encodeURIComponent(profile)}`;
  const migrated = await getJson(globalUrl);
  expect(migrated.theme.name).toBe("dracula");
  expect(migrated.session.confirm_before_quit).toBe(false);
  expect(migrated.session.session_id_poller_max_threads).toBe(12);
  expect(migrated.web.notify_on_idle).toBe(true);
  const profileBefore = readFileSync(join(appDir, "profiles", "work", "config.toml"), "utf8");
  expect(profileBefore).not.toContain("sidebar_position");
  expect(profileBefore).not.toContain("confirm_before_quit");
  const staleOverride = await fetch(profileUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session: { sidebar_position: "right", default_tool: "claude" } }),
  });
  expect(staleOverride.status).toBe(400);
  expect((await staleOverride.json()).message).toContain("session.sidebar_position");
  expect(readFileSync(join(appDir, "profiles", "work", "config.toml"), "utf8")).toBe(profileBefore);
  const logUrl = `${serve.baseUrl}/api/log-level`;
  const runtimeLog = await fetch(logUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ level: "debug" }),
  });
  expect(runtimeLog.ok).toBe(true);
  const { current: temporaryFilter } = await runtimeLog.json();

  await page.goto(`${serve.baseUrl}/settings/session`);
  const position = labelledSelect(page, /^Sidebar Position$/);
  await expect(position).toHaveValue("left");
  const [saveResponse] = await Promise.all([
    page.waitForResponse((response) => response.url() === globalUrl && response.request().method() === "PATCH"),
    position.selectOption("right"),
  ]);
  expect(saveResponse.ok()).toBe(true);

  for (const url of [globalUrl, effectiveUrl]) {
    const saved = await fetch(url).then((r) => r.json());
    expect(saved.session.sidebar_position).toBe("right");
  }
  const overrides = await fetch(profileUrl).then((r) => r.json());
  expect(overrides.session.sidebar_position).toBeUndefined();
  expect(overrides.session.default_tool).toBe("codex");
  expect(overrides.theme.idle_decay_minutes).toBe(5);
  const logStatus = await fetch(logUrl).then((r) => r.json());
  expect(logStatus.current).toBe(temporaryFilter);

  await serve.restart();
  await page.reload();
  await expect(position).toHaveValue("right");
  const persisted = await fetch(globalUrl).then((r) => r.json());
  expect(persisted.session.sidebar_position).toBe("right");
});

test("clearing a logging target removes its global override", async ({ serve, page }) => {
  await page.goto(`${serve.baseUrl}/settings/logging`);
  const target = labelledSelect(page, /^acp\.protocol$/);
  await expect(target).toBeVisible();
  await target.selectOption("debug");
  const globalUrl = `${serve.baseUrl}/api/settings`;
  await expect(async () => {
    const saved = await fetch(globalUrl).then((r) => r.json());
    expect(saved.logging.targets["acp.protocol"]).toBe("debug");
  }).toPass({ timeout: 5_000 });

  await target.selectOption("");
  await expect(async () => {
    const saved = await fetch(globalUrl).then((r) => r.json());
    expect(saved.logging.targets).toEqual({});
  }).toPass({ timeout: 5_000 });
  await page.reload();
  await expect(target).toHaveValue("");
});
