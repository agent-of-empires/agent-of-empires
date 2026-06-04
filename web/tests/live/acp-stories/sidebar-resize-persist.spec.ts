// User story: drag the sidebar resize handle; width persists across
// reload.
//
// WorkspaceSidebar.tsx exposes data-testid="sidebar-resize-handle";
// the global mousemove/mouseup handlers in the same component compute
// the new width and persist to localStorage key "aoe-sidebar-width"
// on release.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base("sidebar width persists across reload after dragging the handle", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-sidebar-resize" }),
  });

  try {
    await page.goto(serve.baseUrl);
    const handle = page.locator('[data-testid="sidebar-resize-handle"]');
    await expect(handle).toBeVisible({ timeout: 10_000 });

    const box = await handle.boundingBox();
    if (!box) throw new Error("handle has no bounding box");

    const startX = box.x + box.width / 2;
    const y = box.y + box.height / 2;
    const targetX = startX + 60;

    const storedBefore = await page.evaluate(() =>
      localStorage.getItem("aoe-sidebar-width"),
    );

    await page.mouse.move(startX, y);
    await page.mouse.down();
    await page.mouse.move(targetX, y, { steps: 5 });
    await page.mouse.up();

    const storedAfter = await page.evaluate(() =>
      localStorage.getItem("aoe-sidebar-width"),
    );
    expect(storedAfter).not.toBeNull();
    const widthAfter = parseFloat(storedAfter!);
    expect(widthAfter).toBeGreaterThan(0);
    // Drag must change the persisted width, not just leave the
    // pre-existing value alone.
    expect(storedAfter).not.toBe(storedBefore);

    await page.reload();
    const storedReloaded = await page.evaluate(() =>
      localStorage.getItem("aoe-sidebar-width"),
    );
    expect(storedReloaded).toBe(storedAfter);
  } finally {
    await serve.stop();
  }
});
