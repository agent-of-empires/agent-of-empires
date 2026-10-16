import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";

test("arrow keys move focus through the session list and Enter opens the focused session", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(3);
  await rows.nth(0).focus();

  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(1)).toBeFocused();
  await expect(page).toHaveURL("/");

  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(2)).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(2)).toBeFocused();

  await page.keyboard.press("ArrowUp");
  await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
});
