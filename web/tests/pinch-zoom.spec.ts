import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import {
  mockTerminalApis,
  installTerminalSpies,
  readFontSize,
  seedSettings,
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

  async function fontSizeWrites(page: Page) {
    return page.evaluate(() =>
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__.filter(
        (w) => w.includes("mobileFontSize") || w.includes("desktopFontSize"),
      ),
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

  test("a two-finger pan does not zoom; pinches clamp at MAX_FONT_SIZE and MIN_FONT_SIZE", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 10 });
    await page.reload();
    await openSession(page);

    expect(await readFontSize(page, "mobile")).toBe(10);
    const wsBefore = await wsCount(page);
    // Clear the write log from the seed/initial settings.
    await page.evaluate(() => {
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__ = [];
    });

    // A vertical two-finger pan is scroll mode, not pinch: no font-size write.
    const range = (n: number) => Array.from({ length: n }, (_, i) => i + 1);
    await twoFingerGesture(
      page,
      [160, 100, 50],
      range(12).map((step) => [100 + step * 16, 50]),
    );
    expect(await fontSizeWrites(page)).toEqual([]);
    expect(await readFontSize(page, "mobile")).toBe(10);

    // Spread 80px -> 464px: 5.8x ratio. 10 * 5.8 = 58 -> clamped to 28.
    await twoFingerGesture(
      page,
      [160, 200, 40],
      range(16).map((step) => [200, 40 + step * 12]),
    );
    await expect.poll(() => readFontSize(page, "mobile"), { timeout: 2_000 }).toBe(28);

    // Pinch 240px -> 48px: 0.2x ratio. 28 * 0.2 = 5.6 -> clamped to 6.
    await twoFingerGesture(
      page,
      [160, 200, 120],
      range(16).map((step) => [200, 120 - step * 6]),
    );
    await expect.poll(() => readFontSize(page, "mobile"), { timeout: 2_000 }).toBe(6);
    // Same session, no reconnect.
    expect(await wsCount(page)).toBe(wsBefore);
  });

  test("touchcancel mid-pinch still persists the latest size", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 10 });
    await page.reload();
    await openSession(page);

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
    // A system gesture / incoming call cancels the touch instead of lifting it.
    await fireTouches(page, "touchcancel", []);

    await expect.poll(() => readFontSize(page, "mobile"), { timeout: 2_000 }).toBeGreaterThan(10);
  });
});
