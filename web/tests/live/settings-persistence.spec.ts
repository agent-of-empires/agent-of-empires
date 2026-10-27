// Settings persist through the real server: REST round-trips, the schema UI, and the theme picker.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { Locator, Page } from "@playwright/test";
import {
  test,
  expect,
  authHeaders,
  bootDashboard,
  confirmPassphraseElevation,
  type ServeHandle,
} from "../helpers/liveTest";
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

test("startup recovery wake stays opt-in and persists per profile without changing restart text", async ({
  serve,
  page,
}, testInfo) => {
  const profile = await defaultProfile(serve);
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const machineUrl = `${settingsUrl}?layer=machine`;
  const effectiveUrl = `${settingsUrl}?profile=${encodeURIComponent(profile)}`;
  const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`;
  const message = "wake up: pick up what you were doing";
  expect((await getJson(machineUrl)).session).toMatchObject({
    startup_recovery_wake: false,
    restart_wake_message: message,
  });
  await page.goto(`${serve.baseUrl}/settings/session`);
  const toggle = page.locator('[data-settings-field="session.startup_recovery_wake"]').getByRole("switch");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  const invalid = await fetch(profileUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session: { startup_recovery_wake: "true" } }),
  });
  expect(invalid.status).toBe(400);
  const saved = page.waitForResponse(
    (response) => response.url().startsWith(settingsUrl) && response.request().method() === "PATCH",
  );
  await toggle.click();
  expect((await saved).ok()).toBe(true);
  expect((await getJson(profileUrl)).session.startup_recovery_wake).toBe(true);
  expect((await getJson(machineUrl)).session.startup_recovery_wake).toBe(false);
  expect((await getJson(effectiveUrl)).session).toMatchObject({
    startup_recovery_wake: true,
    restart_wake_message: message,
  });
  await serve.restart();
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.scrollIntoViewIfNeeded();
  await testInfo.attach("startup-recovery-wake-setting", {
    body: await page.screenshot(),
    contentType: "image/png",
  });
  const reset = await fetch(profileUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session: { startup_recovery_wake: null } }),
  });
  expect(reset.ok).toBe(true);
  expect((await getJson(effectiveUrl)).session).toMatchObject({
    startup_recovery_wake: false,
    restart_wake_message: message,
  });
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "false");
});

test("MCP switch persists globally and changes route availability only after restart", async ({ serve, page }) => {
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const machineUrl = `${settingsUrl}?layer=machine`;
  const mcpList = () =>
    fetch(`${serve.baseUrl}/mcp`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/json, text/event-stream" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
    });
  await page.goto(`${serve.baseUrl}/settings/notifications`);
  const toggle = page.locator('[data-settings-field="web.mcp_enabled"]').getByRole("switch");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  expect([404, 405]).toContain((await mcpList()).status);
  await toggle.click();
  await expect.poll(async () => (await getJson(machineUrl)).web.mcp_enabled).toBe(true);
  expect([404, 405]).toContain((await mcpList()).status);
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");

  const created = await fetch(`${serve.baseUrl}/api/profiles`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "mcp-other" }),
  });
  expect(created.ok).toBe(true);
  expect((await getJson(`${settingsUrl}?profile=mcp-other`)).web.mcp_enabled).toBe(true);
  const profileUrl = `${serve.baseUrl}/api/profiles/mcp-other/settings`;
  expect((await getJson(profileUrl)).web?.mcp_enabled).toBeUndefined();
  const rejected = await fetch(profileUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ web: { mcp_enabled: false } }),
  });
  expect(rejected.status).toBe(400);
  expect((await getJson(machineUrl)).web.mcp_enabled).toBe(true);

  await serve.restart();
  const enabled = await mcpList();
  expect(enabled.status).toBe(200);
  expect((await enabled.json()).result.tools.map((tool: { name: string }) => tool.name)).toContain("list_sessions");
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.click();
  await expect.poll(async () => (await getJson(machineUrl)).web.mcp_enabled).toBe(false);
  expect((await mcpList()).status).toBe(200);
  await serve.restart();
  expect([404, 405]).toContain((await mcpList()).status);
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

  for (const ingress of ["direct TCP", "proxy-reported remote TCP"] as const) {
    test(`sandbox image changes require browser elevation over ${ingress}`, async ({ spawnServe, page }) => {
      const proxied = ingress === "proxy-reported remote TCP";
      const handle = await spawnServe({
        authMode: "passphrase",
        preloginViaHarness: true,
        ...(proxied ? { extraArgs: ["--behind-proxy", "--allowed-host", "127.0.0.1"] } : {}),
      });
      if (proxied) await page.setExtraHTTPHeaders({ "X-Forwarded-For": "203.0.113.10" });
      const profile = await defaultProfile(handle);
      const profileUrl = `${handle.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`;
      const savedImage = async () => (await getJson(profileUrl, handle))?.sandbox?.default_image;
      const baseline = await savedImage();
      await bootDashboard(page, handle, "/settings/sandbox");
      const status = await page.evaluate(async () => (await fetch("/api/login/status")).json());
      expect(status).toMatchObject({ required: true, authenticated: true, elevated: false });
      expect(status.principal).not.toBe("local_owner");

      // Proxy address claims cannot manufacture kernel-authenticated Unix owner authority.
      const spoofed = await fetch(profileUrl, {
        method: "PATCH",
        headers: {
          ...authHeaders(handle),
          "Content-Type": "application/json",
          "X-Forwarded-For": "127.0.0.1",
          "CF-Connecting-IP": "127.0.0.1",
          "X-Real-IP": "127.0.0.1",
          Forwarded: "for=127.0.0.1",
        },
        body: JSON.stringify({ sandbox: { default_image: "ghcr.io/example/img:spoofed-owner" } }),
      });
      expect(spoofed.status).toBe(403);
      expect(await spoofed.json()).toMatchObject({ error: "elevation_required" });
      expect(await savedImage()).toBe(baseline);

      const profilePicker = page.getByTestId("settings-profile-picker").locator("select");
      await expect(profilePicker).toHaveValue(profile);
      const image = page.locator('[data-settings-field="sandbox.default_image"]').locator('input[type="text"]');
      await expect(image).toBeVisible({ timeout: 10_000 });
      await image.fill("ghcr.io/example/img:blocked");
      await image.blur();
      await expect(confirmPassphraseDialog(page)).toBeVisible({ timeout: 5_000 });
      expect(await savedImage()).toBe(baseline);

      await confirmPassphraseElevation(page, handle);
      expect(await savedImage()).toBe(baseline);
      await image.fill("ghcr.io/example/img:elevated");
      await image.blur();
      await expect.poll(savedImage).toBe("ghcr.io/example/img:elevated");
      await expect(image).toHaveValue("ghcr.io/example/img:elevated");
      await expect(confirmPassphraseDialog(page)).toHaveCount(0);
      await bootDashboard(page, handle, "/settings/sandbox");
      await expect(image).toHaveValue("ghcr.io/example/img:elevated", { timeout: 10_000 });
    });
  }
});

test("global settings migrate from profiles and reject stale profile writes", async ({ spawnServe, page }) => {
  let appDir = "";
  const serve = await spawnServe({
    seedFn: ({ home, xdg }) => {
      appDir = appDirFor(home, xdg, resolveAoeBinary());
      writeFileSync(join(appDir, ".schema_version"), "29");
      writeFileSync(
        join(appDir, "config.toml"),
        "default_profile = 'work'\n[theme]\nname = 'empire'\n[systemd]\nenabled = false\n",
      );
      for (const name of ["alpha", "work"]) mkdirSync(join(appDir, "profiles", name), { recursive: true });
      writeFileSync(join(appDir, "profiles", "alpha", "config.toml"), "[theme]\nname = 'rose-pine'\n");
      writeFileSync(
        join(appDir, "profiles", "work", "config.toml"),
        "[theme]\nname = 'dracula'\nidle_decay_minutes = 5\n[session]\nsidebar_position = 'left'\nconfirm_before_quit = false\nsession_id_poller_max_threads = 12\ndefault_tool = 'codex'\n[web]\nnotify_on_idle = true\n",
      );
    },
  });
  const profile = await defaultProfile(serve);
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const machineUrl = `${settingsUrl}?layer=machine`;
  const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`;
  const effectiveUrl = `${settingsUrl}?profile=${encodeURIComponent(profile)}`;
  const migrated = await getJson(machineUrl);
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
  // Sidebar Position is TUI-only, so it sits in the Terminal UI fold.
  await page.getByRole("button", { name: /Terminal UI/ }).click();
  const position = labelledSelect(page, /^Sidebar Position$/);
  await expect(position).toHaveValue("left");
  const [saveResponse] = await Promise.all([
    page.waitForResponse(
      (response) => response.url().startsWith(settingsUrl) && response.request().method() === "PATCH",
    ),
    position.selectOption("right"),
  ]);
  expect(saveResponse.ok()).toBe(true);

  // A global-only field lands machine-wide even though the save names the profile.
  for (const url of [machineUrl, effectiveUrl]) {
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
  await page.getByRole("button", { name: /Terminal UI/ }).click();
  await expect(position).toHaveValue("right");
  const persisted = await fetch(machineUrl).then((r) => r.json());
  expect(persisted.session.sidebar_position).toBe("right");
});

test("shortcut bar toggle persists globally from Terminal UI settings", async ({ serve, page }) => {
  const profile = await defaultProfile(serve);
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const machineUrl = `${settingsUrl}?layer=machine`;
  const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(profile)}/settings`;
  const toggle = page.getByText("Show shortcut bar", { exact: true }).locator("../..").getByRole("switch");

  await page.goto(`${serve.baseUrl}/settings/session`);
  await page.getByRole("button", { name: /Terminal UI/ }).click();
  await expect(toggle).toBeChecked();
  const [saved] = await Promise.all([
    page.waitForResponse(
      (response) => response.url().startsWith(settingsUrl) && response.request().method() === "PATCH",
    ),
    toggle.click(),
  ]);
  expect(saved.ok()).toBe(true);
  expect((await getJson(machineUrl)).session.show_shortcut_bar).toBe(false);
  expect((await getJson(profileUrl)).session?.show_shortcut_bar).toBeUndefined();
  const rejected = await fetch(profileUrl, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ session: { show_shortcut_bar: true } }),
  });
  expect(rejected.status).toBe(400);

  await serve.restart();
  await page.reload();
  await page.getByRole("button", { name: /Terminal UI/ }).click();
  await expect(toggle).not.toBeChecked();
  await toggle.click();
  await expect.poll(async () => (await getJson(machineUrl)).session.show_shortcut_bar).toBe(true);
});

test("runtime logging overrides persist and clearing one preserves the other", async ({ serve, page }) => {
  await page.goto(`${serve.baseUrl}/settings/logging`);
  const uds = labelledSelect(page, /^runtime\.uds$/);
  const ws = labelledSelect(page, /^server\.lifecycle$/);
  const migration = labelledSelect(page, /^migrations\.v043$/);
  await expect(uds).toBeVisible();
  await uds.selectOption("warn");
  const globalUrl = `${serve.baseUrl}/api/settings`;
  await expect.poll(async () => (await getJson(globalUrl)).logging.targets["runtime.uds"]).toBe("warn");
  await ws.selectOption("debug");
  await expect
    .poll(async () => (await getJson(globalUrl)).logging.targets)
    .toEqual({ "runtime.uds": "warn", "server.lifecycle": "debug" });
  await migration.selectOption("info");
  await expect.poll(async () => (await getJson(globalUrl)).logging.targets["migrations.v043"]).toBe("info");
  await page.reload();
  await expect(uds).toHaveValue("warn");
  await expect(ws).toHaveValue("debug");
  await expect(migration).toHaveValue("info");

  await migration.selectOption("");
  await expect
    .poll(async () => (await getJson(globalUrl)).logging.targets)
    .toEqual({ "runtime.uds": "warn", "server.lifecycle": "debug" });
  await serve.restart();
  await page.reload();
  await expect(uds).toHaveValue("warn");
  await expect(ws).toHaveValue("debug");
  await expect(migration).toHaveValue("");
});

test("systemd settings save machine-wide and refuse an invalid slice", async ({ serve, page }) => {
  const settingsUrl = `${serve.baseUrl}/api/settings`;
  const machineUrl = `${settingsUrl}?layer=machine`;
  const savePatch = () =>
    page.waitForResponse(
      (response) => response.url().startsWith(settingsUrl) && response.request().method() === "PATCH",
    );
  const slice = page
    .locator("label", { hasText: /^Systemd slice$/ })
    .locator("..")
    .locator("input");
  const toggle = (label: string) => page.getByText(label, { exact: true }).locator("../..").getByRole("switch");
  // The harness seeds `enabled = false`, so the spec never turns real scopes on.
  const enabled = toggle("Use systemd scopes");
  const survive = toggle("Sessions survive daemon restart");

  await page.goto(`${serve.baseUrl}/settings/systemd`);
  await expect(enabled).not.toBeChecked();
  await expect(slice).toHaveValue("background-aoe-{profile}.slice");
  await slice.fill("aoe-{profile}.slice");
  const [sliceSaved] = await Promise.all([savePatch(), slice.press("Enter")]);
  expect(sliceSaved.ok()).toBe(true);
  await expect(survive).toBeChecked();
  const [toggleSaved] = await Promise.all([savePatch(), survive.click()]);
  expect(toggleSaved.ok()).toBe(true);
  const saved = (await getJson(machineUrl)).systemd;
  expect(saved.slice).toBe("aoe-{profile}.slice");
  expect(saved.survive_daemon_restart).toBe(false);
  expect(saved.enabled).toBe(false);

  await slice.fill("aoe.service");
  const [rejected] = await Promise.all([savePatch(), slice.press("Enter")]);
  expect(rejected.status()).toBe(400);
  expect((await rejected.json()).message).toContain("systemd.slice");
  await expect(page.getByTestId("settings-header-save-error")).toBeVisible();
  await expect(slice).toHaveValue("aoe-{profile}.slice");
  expect((await getJson(machineUrl)).systemd.slice).toBe("aoe-{profile}.slice");

  await serve.restart();
  await page.reload();
  await expect(slice).toHaveValue("aoe-{profile}.slice");
  await expect(survive).not.toBeChecked();
});
