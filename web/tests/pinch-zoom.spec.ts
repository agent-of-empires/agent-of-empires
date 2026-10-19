import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  readFontSize,
  readMachineFontSize,
  fireTouches,
} from "./helpers/terminal-mocks";

test.use({ ...devices["iPhone 13"] });

test.describe("Terminal pinch zoom (mobile)", () => {
  async function openSession(page: Page) {
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  }

  async function wsCount(page: Page) {
    return page.evaluate(() => (window as unknown as { __WS_COUNT__: number }).__WS_COUNT__);
  }

  async function legacyFontSizeWrites(page: Page) {
    return page.evaluate(() =>
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__.filter((w) => w.includes("mobileFontSize")),
    );
  }

  async function twoFingerGesture(page: Page, from: [number, number, number], steps: Array<[number, number]>) {
    const [cx, cy, spread] = from;
    await fireTouches(page, "touchstart", [
      { x: cx - spread, y: cy },
      { x: cx + spread, y: cy },
    ]);
    for (const [y, s] of steps) {
      await fireTouches(page, "touchmove", [
        { x: cx - s, y },
        { x: cx + s, y },
      ]);
    }
    await fireTouches(page, "touchend", []);
  }

  test("defaults to 14px and uses configured machine settings instead of stale local storage", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    expect(await readFontSize(page)).toBe(14);
    expect(await readMachineFontSize(page)).toBeNull();

    await page.evaluate(() => {
      localStorage.setItem("aoe-web-settings", JSON.stringify({ mobileFontSize: 8 }));
    });
    await page.route("**/api/settings?layer=machine", async (route) => {
      if (route.request().method() === "GET") {
        return route.fulfill({ json: { web: { mobile_terminal_font_size: 18 } } });
      }
      return route.fallback();
    });
    await page.reload();
    await openSession(page);

    expect(await readFontSize(page)).toBe(18);
    expect(await readMachineFontSize(page)).toBe(18);
  });

  test("a two-finger pan does not zoom; pinches clamp and persist across reload", async ({ page }) => {
    await installTerminalSpies(page);
    const apis = await mockTerminalApis(page, { mobileTerminalFontSize: 10 });
    await page.route("**/api/app-state/web-ui-state", (route) =>
      route.fulfill({
        json: { "aoe-web-settings": JSON.stringify({ mobileFontSize: 8, desktopFontSize: 14 }) },
      }),
    );
    await page.goto("/");
    await page.evaluate(() => {
      localStorage.setItem("aoe-web-settings", JSON.stringify({ mobileFontSize: 8 }));
    });
    await page.reload();
    await openSession(page);

    expect(await readFontSize(page)).toBe(10);
    const wsBefore = await wsCount(page);
    // Clear the write log from initial settings; the legacy key must never be written by zoom.
    await page.evaluate(() => {
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__ = [];
    });

    // A vertical two-finger pan is scroll mode, not pinch: no font-size change or API write.
    const range = (n: number) => Array.from({ length: n }, (_, i) => i + 1);
    await twoFingerGesture(
      page,
      [160, 100, 50],
      range(12).map((step) => [100 + step * 16, 50]),
    );
    expect(await readMachineFontSize(page)).toBe(10);
    expect(await readFontSize(page)).toBe(10);
    expect(await legacyFontSizeWrites(page)).toEqual([]);
    expect(apis.machineSettingsWrites).toHaveLength(0);

    // Spread 80px -> 464px: 5.8x ratio. 10 * 5.8 = 58 -> clamped to 28.
    const maxWrite = page.waitForResponse((response) => {
      const request = response.request();
      return (
        request.url().includes("/api/settings/mobile-terminal-font-size") &&
        request.method() === "PATCH" &&
        request.postData()?.includes('"font_size":28') === true
      );
    });
    await twoFingerGesture(
      page,
      [160, 200, 40],
      range(16).map((step) => [200, 40 + step * 12]),
    );
    await expect.poll(() => readFontSize(page), { timeout: 2_000 }).toBe(28);
    expect((await maxWrite).request().postDataJSON()).toEqual({ font_size: 28 });
    expect(await readMachineFontSize(page)).toBe(28);

    // Pinch 240px -> 48px: 0.2x ratio. 28 * 0.2 = 5.6 -> clamped to 6.
    const minWrite = page.waitForResponse((response) => {
      const request = response.request();
      return (
        request.url().includes("/api/settings/mobile-terminal-font-size") &&
        request.method() === "PATCH" &&
        request.postData()?.includes('"font_size":6') === true
      );
    });
    await twoFingerGesture(
      page,
      [160, 200, 120],
      range(16).map((step) => [200, 120 - step * 6]),
    );
    await expect.poll(() => readFontSize(page), { timeout: 2_000 }).toBe(6);
    expect((await minWrite).request().postDataJSON()).toEqual({ font_size: 6 });
    expect(await readMachineFontSize(page)).toBe(6);
    expect(await legacyFontSizeWrites(page)).toEqual([]);
    // Same session, no reconnect.
    expect(await wsCount(page)).toBe(wsBefore);

    await page.reload();
    await openSession(page);
    expect(await readFontSize(page)).toBe(6);
    expect(await readMachineFontSize(page)).toBe(6);
    expect(
      await page.evaluate(() => JSON.parse(localStorage.getItem("aoe-web-settings") ?? "{}").mobileFontSize),
    ).toBeUndefined();
  });

  test("touchcancel mid-pinch still persists the latest size", async ({ page }) => {
    await mockTerminalApis(page, { mobileTerminalFontSize: 10 });
    await page.goto("/");
    await openSession(page);
    await expect.poll(() => readFontSize(page)).toBe(10);

    const cx = 160;
    const cy = 200;
    await fireTouches(page, "touchstart", [
      { x: cx - 40, y: cy },
      { x: cx + 40, y: cy },
    ]);
    for (let step = 1; step <= 8; step++) {
      const spread = 40 + step * 10;
      await fireTouches(page, "touchmove", [
        { x: cx - spread, y: cy },
        { x: cx + spread, y: cy },
      ]);
    }
    const saved = page.waitForResponse(
      (response) =>
        response.url().includes("/api/settings/mobile-terminal-font-size") &&
        response.request().method() === "PATCH" &&
        response.status() === 200,
    );
    // A system gesture / incoming call cancels the touch instead of lifting it.
    await fireTouches(page, "touchcancel", []);

    await expect.poll(() => readFontSize(page), { timeout: 2_000 }).toBeGreaterThan(10);
    expect((await saved).request().postDataJSON()).toEqual({ font_size: 28 });
    expect(await readMachineFontSize(page)).toBe(28);
  });
});
