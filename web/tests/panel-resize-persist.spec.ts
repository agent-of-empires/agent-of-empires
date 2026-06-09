// User stories: drag the sidebar / right-panel resize handles; the new
// width persists to localStorage and survives a reload. Ported from the
// live acp-stories (sidebar-resize-persist, right-panel-resize-persist);
// both flows are pure client state (global mousemove/mouseup handlers
// writing "aoe-sidebar-width" / "aoe-split-ratio"), so the stubbed /api
// surface reproduces them faithfully.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const SIDEBAR_WIDTH_KEY = "aoe-sidebar-width";
const SPLIT_STORAGE_KEY = "aoe-split-ratio";

test("sidebar width persists across reload after dragging the handle", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: [{ id: "s-a", title: "story-sidebar-resize", project_path: "/tmp/repo", branch: "feat/a" }],
  });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const handle = page.locator('[data-testid="sidebar-resize-handle"]');
  await expect(handle).toBeVisible();

  const box = await handle.boundingBox();
  if (!box) throw new Error("handle has no bounding box");

  const startX = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  const targetX = startX + 60;

  const storedBefore = await page.evaluate((k) => localStorage.getItem(k), SIDEBAR_WIDTH_KEY);

  await page.mouse.move(startX, y);
  await page.mouse.down();
  await page.mouse.move(targetX, y, { steps: 5 });
  await page.mouse.up();

  // The write happens inside a React functional updater during the
  // global mouseup handler, so it can land a tick after mouse.up()
  // returns; poll instead of reading once and racing the batch.
  await expect
    .poll(() => page.evaluate((k) => localStorage.getItem(k), SIDEBAR_WIDTH_KEY), { timeout: 5_000 })
    .not.toBe(storedBefore);

  const storedAfter = await page.evaluate((k) => localStorage.getItem(k), SIDEBAR_WIDTH_KEY);
  expect(storedAfter).not.toBeNull();
  const widthAfter = parseFloat(storedAfter!);
  expect(widthAfter).toBeGreaterThan(0);

  await page.reload();
  const storedReloaded = await page.evaluate((k) => localStorage.getItem(k), SIDEBAR_WIDTH_KEY);
  expect(storedReloaded).toBe(storedAfter);
});

test("right panel width persists across reload after dragging the handle", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.goto("/");
  await clickSidebarSession(page, "pinch-test");
  await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });

  const handle = page.locator('[data-testid="content-split-resize-handle"]');
  await expect(handle).toBeVisible({ timeout: 10_000 });

  const box = await handle.boundingBox();
  if (!box) throw new Error("handle has no bounding box");

  const startX = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  const targetX = startX - 80;

  const storedBefore = await page.evaluate((k) => localStorage.getItem(k), SPLIT_STORAGE_KEY);

  await page.mouse.move(startX, y);
  await page.mouse.down();
  await page.mouse.move(targetX, y, { steps: 5 });
  await page.mouse.up();

  // ContentSplit writes "aoe-split-ratio" inside a React functional
  // updater during the mouseup handler, so the localStorage write can
  // be batched a tick after page.mouse.up() returns. Poll until we see
  // a value that differs from the pre-drag snapshot rather than reading
  // once and racing the batch.
  await expect
    .poll(() => page.evaluate((k) => localStorage.getItem(k), SPLIT_STORAGE_KEY), {
      timeout: 5_000,
      intervals: [50, 100, 200, 200],
    })
    .not.toBe(storedBefore);

  const storedAfter = await page.evaluate((k) => localStorage.getItem(k), SPLIT_STORAGE_KEY);
  expect(storedAfter).not.toBeNull();
  const widthAfter = parseInt(storedAfter!, 10);
  // MIN_DIFF_WIDTH in ContentSplit.tsx; the drag widens from the 380px
  // default, so anything below the floor means the clamp regressed.
  expect(widthAfter).toBeGreaterThanOrEqual(280);

  await page.reload();
  const storedReloaded = await page.evaluate((k) => localStorage.getItem(k), SPLIT_STORAGE_KEY);
  expect(storedReloaded).toBe(storedAfter);
});
