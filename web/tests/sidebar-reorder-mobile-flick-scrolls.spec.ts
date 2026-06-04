// A fast vertical scroll-flick on the sidebar must NOT reorder a
// row. TouchSensor `{ delay: 150, tolerance: 8 }` only promotes after
// 150ms with movement under 8px; a flick that exceeds tolerance
// before the delay elapses cancels the activation, so no drag starts
// and no PUT fires (WorkspaceSidebar.tsx:1631, #1419).
//
// Real Playwright `page.touchscreen.tap` is single-finger and only
// supports tap. To synthesize a multi-frame touchmove sequence we
// dispatch raw TouchEvents from `page.evaluate`, per the AGENTS.md
// "Legacy mobile/touch recipe".

import { devices } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import {
  installSidebarMocks,
  threeSessionsInOneRepo,
} from "./helpers/sidebarMocks";

test.use({ ...devices["iPhone 13"] });

test("touch flick on the sidebar does not reorder rows", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.goto("/");
  await page.getByRole("button", { name: "Toggle sidebar" }).click();

  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(3);
  await page.waitForFunction(
    () => {
      const r = document
        .querySelector('[data-testid="sidebar-session-row"]')
        ?.getBoundingClientRect();
      return !!r && r.x >= 0 && r.width > 0;
    },
    null,
    { timeout: 5_000 },
  );

  const box = await rows.nth(0).boundingBox();
  if (!box) throw new Error("row box missing");
  const cx = box.x + box.width / 2;
  const cy = box.y + box.height / 2;

  // touchStart, then three quick vertical touchMoves well past the
  // 8px tolerance, then touchEnd, all within ~60ms total wall time.
  // The TouchSensor sees movement BEFORE the 150ms activation delay
  // elapses and cancels activation. Driven via CDP so the events
  // carry through to dnd-kit's document-level listeners the same way
  // the press-hold recipe does.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchStart",
    touchPoints: [{ x: cx, y: cy, id: 1 }],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchMove",
    touchPoints: [{ x: cx, y: cy + 40, id: 1 }],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchMove",
    touchPoints: [{ x: cx, y: cy + 120, id: 1 }],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchMove",
    touchPoints: [{ x: cx, y: cy + 240, id: 1 }],
  });
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchEnd",
    touchPoints: [],
  });

  await page.waitForTimeout(300);
  expect(handle.puts).toEqual([]);
});
