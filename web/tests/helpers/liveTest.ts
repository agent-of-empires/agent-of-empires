// Playwright fixtures for live-backend tests.
//
// Three serve fixtures map onto the three harness modes:
//   - `serve`          : no-auth, dashboard golden path.
//   - `servePassphrase`: passphrase mode (`--auth=passphrase`); specs
//                        navigate without a session cookie so LoginPage
//                        renders, then drive the form. Fixtures that
//                        want a pre-authed cookie pass
//                        `preloginViaHarness: true` to `spawnAoeServe`
//                        and call `seedAuth(page, handle)` before
//                        navigating.
//   - `serveReadOnly`  : --read-only flag set on aoe serve.
//
// The `page` fixture is wrapped so that when `AOE_COVERAGE=1` is set, the
// `window.__coverage__` object emitted by `vite-plugin-istanbul` is read
// once per test and written as JSON under `web/coverage/playwright/`.
// `web/scripts/merge-coverage.mjs` picks those JSONs up.
//
// Specs do:
//
//   import { test, expect, seedAuth } from "../helpers/liveTest";
//
//   test("dashboard loads", async ({ serve, page }) => {
//     await page.goto(serve.baseUrl);
//     ...
//   });

import { test as base, expect, type Page } from "@playwright/test";
import { spawnAoeServe, type ServeHandle } from "./aoeServe";
import { captureCoverage } from "./coverageCapture";

type LiveFixtures = {
  serve: ServeHandle;
  servePassphrase: ServeHandle;
  serveReadOnly: ServeHandle;
  /**
   * Token-mode fixture: spawns `aoe serve --auth=token` and resolves
   * `handle.authToken` from the daemon-written `serve.token` file.
   * Specs typically navigate to `${baseUrl}/?token=${handle.authToken}`
   * and let `web/src/lib/token.ts` capture it into localStorage.
   * Rotation-aware specs call `spawnAoeServe` directly with
   * `tokenLifetimeSecs` / `tokenGraceSecs` overrides.
   */
  serveToken: ServeHandle;
  /**
   * Structured view fixture. Only supported with `authMode: "none"` today; the
   * harness calls `PATCH /api/acp/master` without a session cookie.
   * If you need passphrase + structured view, call `spawnAoeServe` directly and
   * pass the `sessionCookie` through to the master-enable request.
   */
  serveAcp: ServeHandle;
};

/**
 * Seed the session cookie + device binding secret onto a page so a
 * passphrase-mode `aoe serve` accepts subsequent navigations. Call
 * before `page.goto(handle.baseUrl)`.
 */
export async function seedAuth(
  page: Page,
  handle: ServeHandle,
): Promise<void> {
  if (!handle.sessionCookie) return;
  const url = new URL(handle.baseUrl);
  await page.context().addCookies([
    {
      name: handle.sessionCookie.name,
      value: handle.sessionCookie.value,
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      sameSite: "Strict",
    },
  ]);
  if (handle.deviceBindingSecret) {
    const secret = handle.deviceBindingSecret;
    await page.addInitScript((s) => {
      try {
        // Storage key must match `STORAGE_KEY` in web/src/lib/deviceBinding.ts.
        // The SPA reads this on the first authenticated fetch via
        // `getOrCreateDeviceBindingSecret()`; if it does not find a
        // matching base64url-32-byte value it generates a fresh one,
        // which would not match the binding the harness used at
        // `POST /api/login`, and subsequent requests would 401.
        window.localStorage.setItem("aoe_device_binding_secret_v1", s);
      } catch {
        // localStorage may be unavailable depending on origin state.
      }
    }, secret);
  }
}

export const test = base.extend<LiveFixtures>({
  serve: async ({}, use, testInfo) => {
    const h = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await use(h);
    await h.stop();
  },
  servePassphrase: async ({}, use, testInfo) => {
    const h = await spawnAoeServe({
      authMode: "passphrase",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await use(h);
    await h.stop();
  },
  serveToken: async ({}, use, testInfo) => {
    const h = await spawnAoeServe({
      authMode: "token",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await use(h);
    await h.stop();
  },
  serveReadOnly: async ({}, use, testInfo) => {
    const h = await spawnAoeServe({
      authMode: "none",
      readOnly: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await use(h);
    await h.stop();
  },
  serveAcp: async ({}, use, testInfo) => {
    const h = await spawnAoeServe({
      authMode: "none",
      acp: true,
      // Specs that want a custom script set `FAKE_ACP_SCRIPT` themselves
      // through testInfo.use() overrides or call `spawnAoeServe` directly.
      fakeAcpScript: process.env.FAKE_ACP_SCRIPT,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });
    await use(h);
    await h.stop();
  },
  page: async ({ page }, use, testInfo) => {
    await use(page);
    await captureCoverage(page, testInfo.titlePath.join(" > "));
  },
});

export { expect };
export type { ServeHandle };
