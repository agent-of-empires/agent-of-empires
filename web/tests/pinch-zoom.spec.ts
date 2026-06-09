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
    await page.locator(".xterm").waitFor({ state: "visible", timeout: 10_000 });
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

  test("two-finger spread zooms in and clamps at MAX_FONT_SIZE", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 10 });
    await page.reload();
    await openSession(page);

    expect(await readFontSize(page, "mobile")).toBe(10);
    const wsBefore = await wsCount(page);

    // Start 80px apart, spread to 464px: 5.8x ratio. 10 * 5.8 = 58 → clamped to 28.
    const cx = 160;
    const cy = 200;
    await fireTouches(page, "touchstart", [
      { x: cx - 40, y: cy },
      { x: cx + 40, y: cy },
    ]);
    for (let step = 1; step <= 16; step++) {
      const spread = 40 + step * 12;
      await fireTouches(page, "touchmove", [
        { x: cx - spread, y: cy },
        { x: cx + spread, y: cy },
      ]);
    }
    await fireTouches(page, "touchend", []);

    await expect.poll(() => readFontSize(page, "mobile"), { timeout: 2_000 }).toBe(28);
    // Same session, no reconnect.
    expect(await wsCount(page)).toBe(wsBefore);
  });

  test("two-finger pinch-in clamps at MIN_FONT_SIZE", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 14 });
    await page.reload();
    await openSession(page);

    expect(await readFontSize(page, "mobile")).toBe(14);
    const wsBefore = await wsCount(page);

    // Start 240px apart, pinch to 48px: 0.2x ratio. 14 * 0.2 = 2.8 → clamped to 6.
    const cx = 160;
    const cy = 200;
    await fireTouches(page, "touchstart", [
      { x: cx - 120, y: cy },
      { x: cx + 120, y: cy },
    ]);
    for (let step = 1; step <= 16; step++) {
      const spread = 120 - step * 6;
      await fireTouches(page, "touchmove", [
        { x: cx - spread, y: cy },
        { x: cx + spread, y: cy },
      ]);
    }
    await fireTouches(page, "touchend", []);

    await expect.poll(() => readFontSize(page, "mobile"), { timeout: 2_000 }).toBe(6);
    expect(await wsCount(page)).toBe(wsBefore);
  });

  test("two-finger vertical pan does NOT write to localStorage (scroll lock)", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await seedSettings(page, { mobileFontSize: 10 });
    await page.reload();
    await openSession(page);

    // Clear the write log from the seed/initial settings.
    await page.evaluate(() => {
      (window as unknown as { __LS_WRITES__: string[] }).__LS_WRITES__ = [];
    });

    const cx = 160;
    let cy = 100;
    await fireTouches(page, "touchstart", [
      { x: cx - 50, y: cy },
      { x: cx + 50, y: cy },
    ]);
    for (let step = 1; step <= 12; step++) {
      cy = 100 + step * 16;
      await fireTouches(page, "touchmove", [
        { x: cx - 50, y: cy },
        { x: cx + 50, y: cy },
      ]);
    }
    await fireTouches(page, "touchend", []);

    // No font-size write should have been issued (scroll mode, not pinch).
    expect(await fontSizeWrites(page)).toEqual([]);
    expect(await readFontSize(page, "mobile")).toBe(10);
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
