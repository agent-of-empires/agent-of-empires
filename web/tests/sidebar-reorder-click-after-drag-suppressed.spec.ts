// `useSuppressClickAfterDrag` (WorkspaceSidebar.tsx:274-290,
// 356-371) swallows the synthetic click Chromium dispatches ~250ms
// after `mouseup` on a drag. Without that, the dropped-on row would
// receive a click and navigate, defeating the whole drag.
//
// This story drags gamma onto alpha's position and asserts that the
// URL does NOT change to /session/s-a (alpha's id) even though the
// click would have landed there.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

test("click-after-drag suppression keeps the URL on the source row", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  // Start at "/" (no active session). The synthetic click after the
  // drag release would try to land on alpha's row (the drop target);
  // if the suppressor lets it through, the URL flips to /session/s-a.
  // The contract is "URL stays at /".
  await page.goto("/");

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);

  const sourceBox = await wrappers.nth(2).boundingBox();
  const targetBox = await wrappers.nth(0).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");

  await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });
  await page.mouse.up();

  // The drag fires one PUT. After release, the synthetic click would
  // try to activate alpha (the row under the cursor); the suppressor
  // swallows it so the URL stays on /.
  await expect.poll(() => handle.puts.length, { timeout: 3_000 }).toBe(1);
  await page.waitForTimeout(400);
  expect(new URL(page.url()).pathname).toBe("/");
});
