// dnd-kit MouseSensor `{ distance: 8 }` rejects micro-movements
// (WorkspaceSidebar.tsx:1630). Moving only 4px between mouse-down and
// mouse-up must not start a drag; the order stays put and no PUT
// flies. Locks the 8px threshold from drifting (#1419).

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

test("4px movement does not start a drag", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);

  // Snapshot the visible order via the inner link text.
  const beforeOrder = await wrappers.evaluateAll((els) =>
    els.map((el) => el.querySelector("span.truncate[title]")?.getAttribute("title") ?? ""),
  );
  expect(beforeOrder).toEqual(["alpha", "beta", "gamma"]);

  const box = await wrappers.nth(2).boundingBox();
  if (!box) throw new Error("row box missing");
  await page.mouse.move(box.x + box.width - 4, box.y + box.height / 2);
  await page.mouse.down();
  // 4px below the activation threshold; sensor should not promote.
  await page.mouse.move(box.x + box.width - 4, box.y + box.height / 2 + 4);
  await page.mouse.up();

  // Give the sensor a tick; a regression that drops the threshold
  // check would still fire onDragEnd inside this window.
  await page.waitForTimeout(200);

  const afterOrder = await wrappers.evaluateAll((els) =>
    els.map((el) => el.querySelector("span.truncate[title]")?.getAttribute("title") ?? ""),
  );
  expect(afterOrder).toEqual(beforeOrder);
  expect(handle.puts).toEqual([]);
});
