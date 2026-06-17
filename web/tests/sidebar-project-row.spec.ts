// Mocked coverage for the reworked sidebar project header row (#2207):
//   - the grip grab bar is gone (`sidebar-group-drag-handle` never renders),
//   - the project icon shows next to the name at rest while the fold chevron
//     is hidden, and on row hover they swap (icon hides, chevron reveals),
//   - a per-project total session count `(N)` is always visible, expanded or
//     collapsed,
//   - a stationary click on the header still toggles collapse.
//
// Opacity is driven by Tailwind `group-hover` on the header row, so the two
// layers carry stable testids and we assert their computed opacity.

import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

const HEADER = "[data-testid='sidebar-group-header']";
const ICON = "[data-testid='sidebar-group-icon']";
const CHEVRON = "[data-testid='sidebar-group-fold-chevron']";
const COUNT = "[data-testid='sidebar-group-session-count']";
const ROW = "[data-testid='sidebar-session-row']";

test.describe("sidebar project row rework (#2207)", () => {
  test("no grab bar; icon shown and chevron hidden at rest", async ({ page }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator(HEADER)).toHaveCount(1);
    await expect(page.locator("[data-testid='sidebar-group-drag-handle']")).toHaveCount(0);

    // At rest the icon layer is fully visible and the chevron is hidden.
    await expect(page.locator(ICON)).toHaveCSS("opacity", "1");
    await expect(page.locator(CHEVRON)).toHaveCSS("opacity", "0");
  });

  test("hovering the row swaps the icon for the fold chevron", async ({ page }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await page.locator(HEADER).hover();

    await expect(page.locator(ICON)).toHaveCSS("opacity", "0");
    await expect(page.locator(CHEVRON)).toHaveCSS("opacity", "1");
  });

  test("session count is visible and survives collapse", async ({ page }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    // Three sessions across three branch workspaces in one project.
    await expect(page.locator(ROW)).toHaveCount(3);
    await expect(page.locator(COUNT)).toHaveText("(3)");

    // A stationary click on the header toggles collapse; the count stays.
    await page.locator(COUNT).click();
    await expect(page.locator(ROW)).toHaveCount(0);
    await expect(page.locator(COUNT)).toHaveText("(3)");

    // Click again to expand; rows return.
    await page.locator(COUNT).click();
    await expect(page.locator(ROW)).toHaveCount(3);
  });

  test("a drag on the header does not collapse it (trailing click suppressed)", async ({ page }) => {
    await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await expect(page.locator(ROW)).toHaveCount(3);

    const box = await page.locator(HEADER).boundingBox();
    if (!box) throw new Error("header box missing");
    const x = box.x + 60;
    const y = box.y + box.height / 2;

    // Press, move past the 8px dnd activation threshold and back, then
    // release over the header. The trailing release-click must be swallowed
    // so the project stays expanded rather than collapsing.
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x + 16, y, { steps: 6 });
    await page.mouse.move(x, y, { steps: 6 });
    await page.mouse.up();

    await expect(page.locator(ROW)).toHaveCount(3);
    await expect(page.locator(COUNT)).toHaveText("(3)");
  });
});
