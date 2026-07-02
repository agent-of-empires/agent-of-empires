// Plugin lifecycle mutations from a loopback browser under
// `--auth=passphrase` (#2610).
//
// The loopback bypass paths never insert `AuthenticatedSession`, and
// the handler-side gate (`mutation_gate` in src/server/api/plugins.rs)
// used to treat that as not-elevated: every plugin mutation from
// localhost returned 403 elevation_required, unconditionally, so the
// dashboard looped the passphrase prompt forever (elevate succeeded,
// the retry never could). The fix marks loopback-resolved requests
// `LoopbackTrusted` and the gate honors the #1168 carve-out.
//
// This spec locks the fixed behavior: a passphrase-authenticated
// loopback browser toggles a plugin with zero passphrase prompts. It
// drives enable/disable because it rides the exact same `mutation_gate`
// as update apply/dismiss, install, and uninstall, without needing an
// installable external plugin with a pending update.
//
// Same passphrase-mode navigation caveats as
// settings-persistence-theme-passphrase.spec.ts: land on `/` first,
// then route client-side, so a hard navigation never redirects to
// /login.

import { test as base, expect, type Page } from "@playwright/test";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";
import { seedAuth } from "../helpers/liveTest";

const test = base.extend<{ servePreauthed: ServeHandle }>({
  servePreauthed: async ({}, use, testInfo) => {
    const handle = await spawnAoeServe({
      authMode: "passphrase",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      preloginViaHarness: true,
    });
    await use(handle);
    await handle.stop();
  },
});

function authHeaders(handle: ServeHandle): Record<string, string> {
  const out: Record<string, string> = {};
  if (handle.sessionCookie) {
    out["Cookie"] = `${handle.sessionCookie.name}=${handle.sessionCookie.value}`;
  }
  if (handle.deviceBindingSecret) {
    out["X-Aoe-Device-Binding"] = handle.deviceBindingSecret;
  }
  return out;
}

async function webEnabled(handle: ServeHandle): Promise<boolean> {
  const data: { plugins: { id: string; enabled: boolean }[] } = await fetch(`${handle.baseUrl}/api/plugins`, {
    headers: authHeaders(handle),
  }).then((r) => r.json());
  const web = data.plugins.find((p) => p.id === "aoe.web");
  expect(web, "aoe.web must be present in the live registry").toBeTruthy();
  return web!.enabled;
}

async function bootDashboardAndNavigate(page: Page, handle: ServeHandle, path: string): Promise<void> {
  await seedAuth(page, handle);
  await Promise.all([
    page.waitForResponse((res) => res.url().endsWith("/api/about") && res.status() === 200, { timeout: 10_000 }),
    page.goto(handle.baseUrl),
  ]);
  if (path !== "/") {
    await page.evaluate((target) => {
      window.history.pushState({}, "", target);
      window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
  }
}

test("loopback plugin toggle needs no passphrase elevation", async ({ servePreauthed, page }) => {
  expect(await webEnabled(servePreauthed)).toBe(true);

  await bootDashboardAndNavigate(page, servePreauthed, "/settings/plugins");

  // Track the interceptor event so a race where the dialog closes before
  // the assertion cannot mask a prompt that did fire.
  await page.evaluate(() => {
    (window as unknown as { __elevationFired?: boolean }).__elevationFired = false;
    window.addEventListener("aoe:elevation-required", () => {
      (window as unknown as { __elevationFired?: boolean }).__elevationFired = true;
    });
  });

  const toggle = page.getByLabel("Enable Web Dashboard");
  await expect(toggle).toBeVisible({ timeout: 10_000 });
  await expect(toggle).toBeChecked();
  await toggle.click();

  // The mutation lands server-side; pre-fix this 403'd with
  // elevation_required and the registry never changed.
  await expect(async () => {
    expect(await webEnabled(servePreauthed)).toBe(false);
  }).toPass({ timeout: 5_000 });

  // No passphrase prompt anywhere: neither the dialog nor the event.
  await expect(page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i })).toHaveCount(0);
  const fired = await page.evaluate(
    () => (window as unknown as { __elevationFired?: boolean }).__elevationFired ?? false,
  );
  expect(fired).toBe(false);
});
