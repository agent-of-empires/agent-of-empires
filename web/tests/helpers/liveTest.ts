// Playwright fixtures for live-backend tests. Each fixture spawns an isolated
// `aoe serve` and stops it at teardown; `spawnServe` takes custom options.

import { test as base, expect, type Page } from "@playwright/test";
import { spawnAoeServe, type ServeHandle, type SpawnOptions } from "./aoeServe";
import { attachServeDiagnostics } from "./acp";
import { startCoverage, stopAndWriteCoverage } from "./coverageCapture";

export type ServeOptions = Omit<SpawnOptions, "workerIndex" | "parallelIndex">;

type LiveFixtures = {
  /** Spawn a server with custom options; every handle is stopped at teardown. */
  spawnServe: (opts?: ServeOptions) => Promise<ServeHandle>;
  serve: ServeHandle;
  /** `--auth=passphrase` without a session cookie, so LoginPage renders. */
  servePassphrase: ServeHandle;
  /** `--auth=passphrase` with a harness login; use `bootDashboard` to open it in the browser. */
  servePreauthed: ServeHandle;
  serveReadOnly: ServeHandle;
  /** `--auth=token`; `handle.authToken` holds the daemon-written token. */
  serveToken: ServeHandle;
  /** Structured view enabled against the default fake ACP agent. */
  serveAcp: ServeHandle;
};

/** Seed a harness-minted session cookie and device binding before navigation. */
export async function seedAuth(page: Page, handle: ServeHandle): Promise<void> {
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
    // Key must match `STORAGE_KEY` in web/src/lib/deviceBinding.ts, or the SPA mints a new secret and 401s.
    await page.addInitScript((s) => {
      try {
        window.localStorage.setItem("aoe_device_binding_secret_v1", s);
      } catch {
        // localStorage is unavailable on some origins.
      }
    }, handle.deviceBindingSecret);
  }
}

/** The cookie and device-binding headers the SPA's fetch interceptor adds. */
export function authHeaders(handle: ServeHandle): Record<string, string> {
  const out: Record<string, string> = {};
  if (handle.sessionCookie) out["Cookie"] = `${handle.sessionCookie.name}=${handle.sessionCookie.value}`;
  if (handle.deviceBindingSecret) out["X-Aoe-Device-Binding"] = handle.deviceBindingSecret;
  return out;
}

/**
 * Authenticate the page and open the SPA at `/`, then route client-side: in passphrase mode a hard
 * navigation elsewhere carries no device-binding header and redirects to /login.
 */
export async function bootDashboard(page: Page, handle: ServeHandle, path = "/"): Promise<void> {
  await seedAuth(page, handle);
  // Listen before navigating; bootstrap's /api/about can resolve before goto settles.
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

export const test = base.extend<LiveFixtures>({
  spawnServe: async ({}, use, testInfo) => {
    const handles: Array<{ handle: ServeHandle; acp?: boolean }> = [];
    await use(async (opts = {}) => {
      const handle = await spawnAoeServe({
        ...opts,
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
      });
      handles.push({ handle, acp: opts.acp });
      return handle;
    });
    if (testInfo.status !== testInfo.expectedStatus) {
      for (const { handle, acp } of handles) if (acp) await attachServeDiagnostics(testInfo, handle).catch(() => {});
    }
    const results = await Promise.allSettled(handles.map(({ handle }) => handle.stop()));
    const errors = results.flatMap((r) => (r.status === "rejected" ? [r.reason] : []));
    if (errors.length) throw new AggregateError(errors, "live server teardown failed");
  },
  serve: async ({ spawnServe }, use) => use(await spawnServe()),
  servePassphrase: async ({ spawnServe }, use) => use(await spawnServe({ authMode: "passphrase" })),
  servePreauthed: async ({ spawnServe }, use) =>
    use(await spawnServe({ authMode: "passphrase", preloginViaHarness: true })),
  serveToken: async ({ spawnServe }, use) => use(await spawnServe({ authMode: "token" })),
  serveReadOnly: async ({ spawnServe }, use) => use(await spawnServe({ readOnly: true })),
  serveAcp: async ({ spawnServe }, use) => use(await spawnServe({ acp: true })),
  page: async ({ page }, use, testInfo) => {
    const started = await startCoverage(page);
    await use(page);
    await stopAndWriteCoverage(page, testInfo.titlePath.join(" > "), started);
  },
});

export { expect };
export type { ServeHandle };
