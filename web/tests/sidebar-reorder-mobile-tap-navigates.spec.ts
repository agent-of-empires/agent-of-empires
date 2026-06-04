// On mobile a tap on a row must navigate, not reorder. dnd-kit's
// TouchSensor has `{ delay: 150, tolerance: 8 }` activation
// (WorkspaceSidebar.tsx:1631): a quick tap never crosses the 150ms
// delay, so the sensor never promotes and the inner Link's click
// runs as if the wrapper were not draggable (#1419).

import { devices } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import {
  installSidebarMocks,
  threeSessionsInOneRepo,
} from "./helpers/sidebarMocks";

test.use({ ...devices["iPhone 13"] });

test("touch tap on a row navigates", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.goto("/");
  // On mobile the sidebar is initially closed; open it via the
  // toggle so the rows have a visible bounding box. The dashboard
  // exposes the toggle with accessible name "Toggle sidebar".
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

  const box = await rows.nth(1).boundingBox();
  if (!box) throw new Error("row box missing");
  await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);

  await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });

  // No PUT fired; the gesture never started a drag.
  await page.waitForTimeout(200);
  expect(handle.puts).toEqual([]);
});
