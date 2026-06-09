// A press-and-hold past the 150ms TouchSensor delay, followed by a
// drag past 8px tolerance, DOES reorder on mobile
// (WorkspaceSidebar.tsx:1631). Locks the activation path so the
// mobile drag affordance does not silently break when the dnd-kit
// dependency or its activation constraints change (#1419).
//
// Synthesizes TouchEvents from `page.evaluate` per the AGENTS.md
// "Legacy mobile/touch recipe"; `page.touchscreen.tap` is single-
// finger and does not satisfy the delay sensor cleanly.

import { devices } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo, workspaceId } from "./helpers/sidebarMocks";

test.use({ ...devices["iPhone 13"] });

test("touch press-and-hold drag reorders the row and PUTs the new order", async ({ page }) => {
  const sessions = threeSessionsInOneRepo();
  const handle = await installSidebarMocks(page, { sessions });

  await page.goto("/");
  await page.getByRole("button", { name: "Toggle sidebar" }).click();

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);
  await page.waitForFunction(
    () => {
      const r = document.querySelector("[aria-roledescription='Press and hold to reorder']")?.getBoundingClientRect();
      return !!r && r.x >= 0 && r.width > 0;
    },
    null,
    { timeout: 5_000 },
  );

  const sourceBox = await wrappers.nth(2).boundingBox();
  const targetBox = await wrappers.nth(0).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");
  const sx = sourceBox.x + sourceBox.width / 2;
  const sy = sourceBox.y + sourceBox.height / 2;
  const tx = targetBox.x + targetBox.width / 2;
  const ty = targetBox.y + targetBox.height / 2;

  // Drive real touch events through the Chrome DevTools Protocol.
  // Playwright's `page.touchscreen` only exposes `tap`; raw
  // `Input.dispatchTouchEvent` lets the test hold past the 150ms
  // delay, then walk the touch across the sidebar one frame at a
  // time so dnd-kit's TouchSensor activates and its collision
  // detector resolves to the target row.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchStart",
    touchPoints: [{ x: sx, y: sy, id: 1 }],
  });
  await page.waitForTimeout(220);
  const frames = 8;
  for (let i = 1; i <= frames; i++) {
    const t = i / frames;
    const x = sx + (tx - sx) * t;
    const y = sy + (ty - sy) * t;
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchMove",
      touchPoints: [{ x, y, id: 1 }],
    });
    await page.waitForTimeout(20);
  }
  await cdp.send("Input.dispatchTouchEvent", {
    type: "touchEnd",
    touchPoints: [],
  });

  // One PUT body with gamma's id at the top of the flat order.
  await expect.poll(() => handle.puts.length, { timeout: 4_000 }).toBe(1);
  const sent = handle.puts[0]?.order ?? [];
  expect(sent[0]).toBe(workspaceId(sessions[2]!));
});
