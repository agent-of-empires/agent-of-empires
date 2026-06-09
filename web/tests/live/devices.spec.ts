// Connected-devices view coverage (#1235).
//
// `ConnectedDevices.tsx` lists the persisted login sessions as devices.
// As of #1235 the view is backed by the login-session store, not the
// old IP/user-agent request tracker, so it is exercised under
// `--auth=passphrase` where login sessions actually exist. The harness
// pre-authenticates one session; the panel must render it, flag it as
// "this device", and expose the sign-out-all escape hatch.
//
// Direct `page.goto(/settings/devices)` is avoided in passphrase mode:
// a hard navigation carries only the cookie (no device-binding header
// from the SPA fetch wrapper) and would redirect to /login. Land on `/`
// first (SPA shell), let the interceptor authenticate API calls, then
// switch the route client-side via history.pushState.

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

test("settings -> devices renders the signed-in session as this device", async ({ servePreauthed, page }) => {
  await bootDashboardAndNavigate(page, servePreauthed, "/settings/devices");

  await expect(page.getByRole("heading", { name: /connected devices/i })).toBeVisible({ timeout: 10_000 });

  // The pre-authed session renders as a device flagged "this device".
  await expect(page.getByText(/this device/i)).toBeVisible({ timeout: 10_000 });

  // The sign-out-everywhere escape hatch is present.
  await expect(page.getByRole("button", { name: /sign out all devices/i })).toBeVisible();
});
