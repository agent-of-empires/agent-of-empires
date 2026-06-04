// Theme persistence under `--auth=passphrase` (#1510).
//
// Story 2: a passphrase user picks a theme. The dashboard repaints,
// the new theme survives a page reload AND an `aoe serve` restart,
// and NO passphrase re-prompt fires. Locks the body-shape elevation
// gate in `update_profile_settings`: theme is on the safe list, so
// the handler must not return 403 elevation_required and the client
// must not pop ElevationPrompt.
//
// Story 3: the same passphrase user PATCHes a sandbox image. The
// daemon DOES still return 403 elevation_required and the client
// DOES pop the inline passphrase prompt. Locks the threat-model
// half of the fix: tamper-surface fields stay gated.
//
// Both tests boot a fresh `aoe serve --auth=passphrase` via
// `spawnAoeServe({ preloginViaHarness: true })` and inject the
// resulting session cookie + device binding so the browser starts
// authenticated but NOT elevated. Elevation is the second factor
// the issue is about.
//
// Direct `page.goto(/settings/...)` is avoided in passphrase mode:
// the path is not in `is_login_session_exempt`, so a hard browser
// navigation that carries only the cookie (no device-binding header
// from the SPA's fetch wrapper) would redirect to `/login`. We
// always land on `/` first (exempt; serves the SPA shell), let the
// fetch interceptor authenticate subsequent API calls with cookie +
// binding, then change the route client-side via history.pushState
// + popstate so react-router renders the new view without a
// re-navigation that would 401.

import { test as base, expect, type Page } from "@playwright/test";
import {
  spawnAoeServe,
  type ServeHandle,
} from "../helpers/aoeServe";
import { seedAuth } from "../helpers/liveTest";

const SWITCH_TO = "dracula";

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
  // Test-side fetches bypass the SPA's fetch wrapper, so they must
  // supply the session cookie + device binding header themselves.
  // Mirrors what `fetchInterceptor.attachAuthHeader` adds in the
  // browser.
  const out: Record<string, string> = {};
  if (handle.sessionCookie) {
    out["Cookie"] = `${handle.sessionCookie.name}=${handle.sessionCookie.value}`;
  }
  if (handle.deviceBindingSecret) {
    out["X-Aoe-Device-Binding"] = handle.deviceBindingSecret;
  }
  return out;
}

/**
 * `aoe serve` restart wipes the in-memory `LoginManager` state, so any
 * cookie minted by the previous process becomes invalid: the device
 * binding (32 random bytes) was stored in process memory and is gone.
 * Re-mint a session by POSTing the same passphrase + binding the
 * harness used during initial preauth. Mutates the handle in place so
 * `authHeaders(handle)` returns the fresh cookie. Mirrors
 * `loginWithPassphrase` in `web/tests/helpers/aoeServe.ts`, which is
 * not exported.
 */
async function reLogin(handle: ServeHandle): Promise<void> {
  if (!handle.passphrase || !handle.deviceBindingSecret) return;
  const res = await fetch(`${handle.baseUrl}/api/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      passphrase: handle.passphrase,
      device_binding_secret: handle.deviceBindingSecret,
    }),
  });
  if (!res.ok) {
    throw new Error(`re-login after restart failed: ${res.status} ${await res.text()}`);
  }
  const setCookie = res.headers.get("set-cookie") ?? "";
  const match = /aoe_session=([^;]+)/.exec(setCookie);
  if (!match) {
    throw new Error(`re-login did not set aoe_session cookie. Set-Cookie: ${setCookie}`);
  }
  handle.sessionCookie = { name: "aoe_session", value: match[1] };
}

async function resolveDefaultProfile(handle: ServeHandle): Promise<string> {
  const profiles: Array<{ name: string; is_default?: boolean }> = await fetch(
    `${handle.baseUrl}/api/profiles`,
    { headers: authHeaders(handle) },
  ).then((r) => r.json());
  return profiles.find((p) => p.is_default)?.name ?? profiles[0]?.name ?? "main";
}

async function bootDashboardAndNavigate(
  page: Page,
  handle: ServeHandle,
  path: string,
): Promise<void> {
  await seedAuth(page, handle);
  // Register the response listener BEFORE navigating: the SPA fires
  // /api/about during bootstrap, which can resolve before `page.goto`
  // settles. Attaching the wait afterwards races that and would miss the
  // response, then time out. `Promise.all` attaches the listener first,
  // then triggers the navigation. /api/about is served unconditionally
  // once auth passes, so once it resolves the SPA's cookie+binding pair is
  // known good.
  await Promise.all([
    page.waitForResponse(
      (res) => res.url().endsWith("/api/about") && res.status() === 200,
      { timeout: 10_000 },
    ),
    page.goto(handle.baseUrl),
  ]);
  if (path !== "/") {
    await page.evaluate((target) => {
      window.history.pushState({}, "", target);
      window.dispatchEvent(new PopStateEvent("popstate"));
    }, path);
  }
}

test("theme picker persists across reload + restart without passphrase prompt", async ({
  servePreauthed,
  page,
}) => {
  const defaultProfile = await resolveDefaultProfile(servePreauthed);
  const profileUrl = `${servePreauthed.baseUrl}/api/profiles/${encodeURIComponent(defaultProfile)}/settings`;

  await bootDashboardAndNavigate(page, servePreauthed, "/settings/theme");

  // Listen for the elevation prompt event the fetchInterceptor fires
  // on 403 elevation_required, so a flaky "dialog never opened" race
  // can't silently let the test pass.
  await page.evaluate(() => {
    (window as unknown as { __elevationFired?: boolean }).__elevationFired = false;
    window.addEventListener("aoe:elevation-required", () => {
      (window as unknown as { __elevationFired?: boolean }).__elevationFired = true;
    });
  });

  const themeSelect = page
    .locator("label", { hasText: /^Theme$/ })
    .locator("..")
    .locator("select");
  await expect(themeSelect).toBeVisible({ timeout: 10_000 });
  await expect
    .poll(
      async () =>
        await themeSelect.evaluate((sel: HTMLSelectElement, target) =>
          Array.from(sel.options).some((o) => o.value === target),
        SWITCH_TO),
      { timeout: 5_000 },
    )
    .toBe(true);
  await themeSelect.selectOption(SWITCH_TO);

  await expect(async () => {
    const after = await fetch(profileUrl, { headers: authHeaders(servePreauthed) }).then(
      (r) => r.json(),
    );
    expect(after?.theme?.name).toBe(SWITCH_TO);
  }).toPass({ timeout: 5_000 });

  // No elevation prompt fired anywhere. Checks both the DOM dialog
  // and the event the interceptor would have dispatched.
  await expect(
    page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i }),
  ).toHaveCount(0);
  const fired = await page.evaluate(
    () =>
      (window as unknown as { __elevationFired?: boolean }).__elevationFired ??
      false,
  );
  expect(fired).toBe(false);

  // Client-side repaint after PATCH resolves.
  await expect
    .poll(
      () =>
        page.evaluate(() =>
          document.documentElement.style
            .getPropertyValue("--color-surface-900")
            .trim(),
        ),
      { timeout: 5_000, intervals: [100, 200, 400] },
    )
    .toBe("#282a36");

  // Same register-before-navigate guard as the initial boot: the post-reload
  // /api/about can resolve before `page.reload()` settles, so attach the
  // listener first via `Promise.all`.
  await Promise.all([
    page.waitForResponse(
      (res) => res.url().endsWith("/api/about") && res.status() === 200,
      { timeout: 10_000 },
    ),
    page.reload(),
  ]);
  const afterReload = await fetch(profileUrl, { headers: authHeaders(servePreauthed) }).then(
    (r) => r.json(),
  );
  expect(afterReload?.theme?.name).toBe(SWITCH_TO);

  await servePreauthed.restart();
  await reLogin(servePreauthed);
  const afterRestart = await fetch(profileUrl, { headers: authHeaders(servePreauthed) }).then(
    (r) => r.json(),
  );
  expect(afterRestart?.theme?.name).toBe(SWITCH_TO);
});

test("sandbox image change still requires passphrase elevation", async ({
  servePreauthed,
  page,
}) => {
  const defaultProfile = await resolveDefaultProfile(servePreauthed);

  await bootDashboardAndNavigate(page, servePreauthed, "/");

  // Fire the PATCH from the page so the fetchInterceptor (installed by
  // the SPA bootstrap) sees the 403 and dispatches the elevation event.
  // A direct test-side `fetch` would bypass the interceptor and the
  // dialog would never open even when the server side is correct.
  const status = await page.evaluate(async (profile) => {
    const res = await fetch(
      `/api/profiles/${encodeURIComponent(profile)}/settings`,
      {
        method: "PATCH",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          sandbox: { default_image: "ghcr.io/example/img:tampered" },
        }),
      },
    );
    return res.status;
  }, defaultProfile);
  expect(status).toBe(403);

  // ElevationPrompt opens (from fetchInterceptor dispatching
  // ELEVATION_REQUIRED_EVENT on the 403 elevation_required payload).
  // The dialog element has role=dialog + aria-modal=true but no
  // accessible name (no aria-label, no aria-labelledby pointing at
  // the "Confirm passphrase" header), so `getByRole("dialog", { name })`
  // does not match. Locate by role then filter on visible text instead.
  await expect(
    page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i }),
  ).toBeVisible({ timeout: 5_000 });

  // Server state did not move: the sandbox image stays at whatever it
  // was. Use the seeded cookie + binding so the GET passes the wall.
  const after = await fetch(
    `${servePreauthed.baseUrl}/api/profiles/${encodeURIComponent(defaultProfile)}/settings`,
    { headers: authHeaders(servePreauthed) },
  ).then((r) => r.json());
  expect(after?.sandbox?.default_image ?? "").not.toBe(
    "ghcr.io/example/img:tampered",
  );
});
