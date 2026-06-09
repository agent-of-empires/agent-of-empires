// dnd-kit MouseSensor has `{ distance: 8 }` activation
// (WorkspaceSidebar.tsx:1630). A press-and-release on a row WITHOUT
// movement must not trigger reorder; it must navigate to the session
// instead. Locks the desktop activation threshold from drifting
// silently (#1419).

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

test("stationary click on a row navigates without reordering", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);

  const box = await wrappers.nth(1).boundingBox();
  if (!box) throw new Error("row box missing");
  await page.mouse.move(box.x + box.width - 4, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.up();

  // The Link inside SessionRow resolves to `/session/<id>`; the URL
  // change is the user-observable contract here.
  await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });

  await page.waitForTimeout(200);
  expect(handle.puts).toEqual([]);
});
