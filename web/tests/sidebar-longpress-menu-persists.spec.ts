// A native `contextmenu` event arriving after the long-press timer already
// opened the session-row menu must NOT dismiss it (#3460).
//
// On Android the long-press gesture emits a native `contextmenu` a moment
// after our own 500ms timer has opened and painted the menu. The document
// dismiss listener used to be the bare `close`, with neither the inside-menu
// guard nor the recency guard the sibling `click` listener already had, so the
// menu opened and instantly vanished. Delete, Rename and Stop were then
// unreachable for every touch user.
//
// Chromium does not emit `contextmenu` on a touch hold, so the test drives a
// real CDP touch hold to arm the app timer and then synthesizes the one event
// the engine will not produce. That is the whole point of the repro: the
// ordering (touchstart, timer opens menu, document listener installed, THEN
// contextmenu) is what breaks, and it is not reachable any other way here.
//
// The menu is a portal rendered `position: fixed` at the touch coordinates, so
// it sits directly under the finger. Which element Android targets is not
// guaranteed, so both plausible targets are exercised: the topmost element at
// the press point (asserted to be inside the menu) and the row itself.

import { devices } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";
import { openMobileSidebar } from "./helpers/sidebar";

test.use({ ...devices["iPhone 13"] });

const LONG_PRESS_MS = 500;

test("a native contextmenu after the long-press does not dismiss the row menu", async ({ page }) => {
  await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });

  await page.goto("/");
  // Blocks until the slide-in settles the row box inside the viewport, so the
  // synthesized touch lands on the row rather than off-screen.
  await openMobileSidebar(page);

  const row = page.getByTestId("sidebar-session-row").first();
  await expect(row).toBeVisible();
  const box = await row.boundingBox();
  if (!box) throw new Error("session row has no bounding box");
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;

  const menu = page.getByTestId("sidebar-context-menu");
  const cdp = await page.context().newCDPSession(page);

  // Hold past the 500ms long-press threshold so the app timer opens the menu
  // and the effect installs its document listeners.
  await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y, id: 1 }] });
  await page.waitForTimeout(LONG_PRESS_MS + 100);
  await expect(menu).toBeVisible();

  // The portal is placed at the press point, so it should now be the topmost
  // element there. Pin that, since it is what makes the inside-menu guard the
  // load-bearing half of the fix.
  const topmostIsMenu = await page.evaluate(
    ({ px, py }) => {
      const target = document.elementFromPoint(px, py);
      const el = document.querySelector('[data-testid="sidebar-context-menu"]');
      return !!target && !!el && el.contains(target);
    },
    { px: x, py: y },
  );
  expect(topmostIsMenu).toBe(true);

  // Target 1: the topmost element under the finger, which is inside the menu.
  await page.evaluate(
    ({ px, py }) => {
      document.elementFromPoint(px, py)?.dispatchEvent(
        new MouseEvent("contextmenu", {
          bubbles: true,
          cancelable: true,
          composed: true,
          button: 2,
          clientX: px,
          clientY: py,
        }),
      );
    },
    { px: x, py: y },
  );
  await expect(menu).toBeVisible();

  // Target 2: the row itself, in case the engine reuses the touchstart target
  // instead of hit-testing at dispatch time.
  await row.evaluate(
    (el, point) => {
      el.dispatchEvent(
        new MouseEvent("contextmenu", {
          bubbles: true,
          cancelable: true,
          composed: true,
          button: 2,
          clientX: point.px,
          clientY: point.py,
        }),
      );
    },
    { px: x, py: y },
  );
  await expect(menu).toBeVisible();

  await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });

  // The guard is a window, not a latch: once it lapses a tap outside still
  // dismisses the menu. A tap rather than a mouse click, since touch is the
  // path the bug lived on. The point is derived from the menu box and clears
  // it HORIZONTALLY on purpose: the menu is capped at `100dvh - 16px` and on
  // a phone it is nearly full height, so there is no reliable gap above or
  // below it, and Chromium's touch adjustment snaps a near-miss back onto the
  // menu. The side margin is the only dependable outside.
  await page.waitForTimeout(LONG_PRESS_MS + 100);
  const outside = await menu.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const gapLeft = r.left;
    const gapRight = window.innerWidth - r.right;
    if (Math.max(gapLeft, gapRight) < 12) throw new Error("no horizontal gap beside the menu");
    return {
      x: Math.round(gapLeft >= gapRight ? gapLeft / 2 : (r.right + window.innerWidth) / 2),
      y: Math.round(r.top + r.height / 2),
    };
  });
  const urlBefore = page.url();
  await page.touchscreen.tap(outside.x, outside.y);
  await expect(menu).toBeHidden();
  // The dismissal has to come from the document listener, not from the tap
  // landing on a row and navigating away, which would hide the menu too.
  expect(page.url()).toBe(urlBefore);
});
