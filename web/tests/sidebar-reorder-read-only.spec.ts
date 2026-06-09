// Read-only viewers cannot drag rows: `useSortable({ disabled })` is
// fed `dragOff = !!props.readOnly || !!props.dragDisabled`
// (WorkspaceSidebar.tsx:389-391), `listeners` are not spread onto the
// wrapper, and `aria-roledescription` is omitted (line 423-425). The
// onDragEnd callback is also gated at the DndContext level
// (line 1855). A press-and-hold attempt must produce no visual lift
// and no PUT (#1419).

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

test("read-only viewer cannot drag sidebar rows", async ({ page }) => {
  const handle = await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
    readOnly: true,
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  // The drag-enabled wrapper carries
  // `aria-roledescription="Press and hold to reorder"`. In read-only
  // mode the attribute is omitted; the row should not match at all.
  const dragWrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect.poll(() => dragWrappers.count(), { timeout: 5_000 }).toBe(0);

  // Locate the row a different way and attempt a drag anyway. If a
  // future regression re-enables listeners on the wrapper, this
  // gesture would surface as a PUT body or a visual ring.
  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(3);
  const sourceBox = await rows.nth(2).boundingBox();
  const targetBox = await rows.nth(0).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");

  await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });

  // Visual feedback contract: no row gains the active-drag ring class.
  // The check runs mid-gesture so a regression that wires listeners
  // back up would still show the lift.
  const ringedCount = await page.locator(".ring-2.ring-brand-500").count();
  expect(ringedCount).toBe(0);

  await page.mouse.up();
  await page.waitForTimeout(300);
  expect(handle.puts).toEqual([]);
});
