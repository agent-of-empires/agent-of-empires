// Live-backend theme switching coverage. Spins up a real `aoe serve`
// process via the shared aoeServe harness and drives the dashboard's
// theme picker through actual /api/themes/:name + /api/theme/current
// calls. This is the test that would have caught the
// OnceLock-via-load_theme deadlock that the mock-API tests in
// theme-switch.spec.ts couldn't: mocks bypass the resolver entirely.

import { test, expect, type Page } from "@playwright/test";
import { spawnAoeServe, type ServeHandle } from "../helpers/aoeServe";

// Builtin pinned to a value that visibly differs from Empire so the
// dashboard repaint is observable.
const SWITCH_TO = "dracula";

async function readCssVar(page: Page, name: string): Promise<string> {
  return await page.evaluate((n) => document.documentElement.style.getPropertyValue(n).trim(), name);
}

test.describe("Live aoe serve theme switching (#1189)", () => {
  let handle: ServeHandle;

  test.beforeAll(async ({}, workerInfo) => {
    handle = await spawnAoeServe({
      authMode: "none",
      workerIndex: workerInfo.workerIndex,
      parallelIndex: workerInfo.parallelIndex,
    });
  });

  test.afterAll(async () => {
    await handle?.stop();
  });

  test("GET /api/themes/:name returns within 2s and is not stuck in resolve", async () => {
    // The OnceLock-via-load_theme deadlock made this endpoint hang
    // forever per request. AbortController with a tight budget
    // catches that regression cheaply.
    const ctrl = new AbortController();
    const watchdog = setTimeout(() => ctrl.abort(), 2_000);
    try {
      const res = await fetch(`${handle.baseUrl}/api/themes/${SWITCH_TO}`, {
        signal: ctrl.signal,
      });
      expect(res.ok).toBe(true);
      const body = await res.json();
      expect(body.name).toBe(SWITCH_TO);
      expect(body.source).toBe("builtin");
      expect(body.appearance).toBe("dark");
      expect(body.web.cssVars["--color-surface-900"]).toBe("#282a36");
      expect(body.terminal.cssVars["--term-bg"]).toBe("#282a36");
      expect(body.syntax.shikiTheme).toBe("dracula");
    } finally {
      clearTimeout(watchdog);
    }
  });

  test("GET /api/themes/:name handles all 6 builtins sequentially without hanging", async () => {
    const builtins = ["empire", "phosphor", "tokyo-night-storm", "catppuccin-latte", "dracula", "rose-pine"];
    for (const name of builtins) {
      const ctrl = new AbortController();
      const watchdog = setTimeout(() => ctrl.abort(), 2_000);
      try {
        const res = await fetch(`${handle.baseUrl}/api/themes/${name}`, {
          signal: ctrl.signal,
        });
        expect(res.ok, `${name} did not return`).toBe(true);
        const body = await res.json();
        expect(body.name).toBe(name);
        expect(body.web.cssVars).toBeTruthy();
      } finally {
        clearTimeout(watchdog);
      }
    }
  });

  test("dashboard chrome repaints when theme switches via API", async ({ page }) => {
    await page.goto(`${handle.baseUrl}/`);
    // Wait for the React-side fetch of /api/theme/current to land
    // and seed --color-surface-900 with the active palette.
    await expect
      .poll(
        async () => {
          const v = await readCssVar(page, "--color-surface-900");
          return v.length > 0;
        },
        { timeout: 10_000, intervals: [100, 250, 500] },
      )
      .toBe(true);

    // Drive a theme switch by patching the dedicated global theme
    // endpoint the picker uses, then firing the same event the
    // theme widget's save() handler dispatches.
    const patch = await page.request.patch(`${handle.baseUrl}/api/theme`, {
      data: { name: SWITCH_TO },
    });
    expect(patch.ok()).toBe(true);

    await page.evaluate((name) => {
      window.dispatchEvent(
        new CustomEvent("aoe:theme-picker-changed", {
          detail: { name },
        }),
      );
    }, SWITCH_TO);

    await expect
      .poll(() => readCssVar(page, "--color-surface-900"), {
        timeout: 5_000,
        intervals: [100, 200, 400],
      })
      .toBe("#282a36");

    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    expect(bg).toMatch(/40,\s*42,\s*54/);
  });
});
