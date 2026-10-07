// Dragging the sidebar and right-panel resize handles persists the new width to
// localStorage and survives a reload. Both are pure client state written by
// global mousemove/mouseup handlers, so stubbed /api reproduces them.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const SIDEBAR_WIDTH_KEY = "aoe-sidebar-width";
const SPLIT_STORAGE_KEY = "aoe-split-ratio";

/** Drag `testId`'s handle by `dx` and return the new stored value once the mouseup write lands. */
async function dragHandle(page: Page, testId: string, key: string, dx: number) {
  const handle = page.locator(`[data-testid="${testId}"]`);
  await expect(handle).toBeVisible({ timeout: 10_000 });
  const box = (await handle.boundingBox())!;
  const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];
  const before = await page.evaluate((k) => localStorage.getItem(k), key);
  await page.mouse.move(x, y);
  await page.mouse.down();
  await page.mouse.move(x + dx, y, { steps: 5 });
  await page.mouse.up();
  // The write happens inside a React functional updater during the global
  // mouseup handler, so it can land a tick after mouse.up() returns.
  await expect.poll(() => page.evaluate((k) => localStorage.getItem(k), key), { timeout: 5_000 }).not.toBe(before);
  return (await page.evaluate((k) => localStorage.getItem(k), key))!;
}

test("sidebar and right panel widths persist across reload after dragging their handles", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.goto("/");
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });

  const sidebar = await dragHandle(page, "sidebar-resize-handle", SIDEBAR_WIDTH_KEY, 60);
  expect(parseFloat(sidebar)).toBeGreaterThan(0);
  const split = await dragHandle(page, "content-split-resize-handle", SPLIT_STORAGE_KEY, -80);
  // MIN_DIFF_WIDTH in ContentSplit.tsx; the drag widens from the 380px
  // default, so anything below the floor means the clamp regressed.
  expect(parseInt(split, 10)).toBeGreaterThanOrEqual(280);

  await page.reload();
  expect(await page.evaluate((k) => localStorage.getItem(k), SIDEBAR_WIDTH_KEY)).toBe(sidebar);
  expect(await page.evaluate((k) => localStorage.getItem(k), SPLIT_STORAGE_KEY)).toBe(split);
});
