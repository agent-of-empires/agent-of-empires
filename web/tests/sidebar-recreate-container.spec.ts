// Recreate container: reachable under More in the row menu on desktop and in
// the mobile bottom sheet, and its confirmation fits a phone viewport.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { DESKTOP, iPhone13 } from "./helpers/viewports";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";
import { openMobileSidebar } from "./helpers/sidebar";

const SHOTS = process.env.RECREATE_CONTAINER_SHOTS;

const BOXED: MockSessionInput[] = [
  { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a", group: "/tmp/repo-a", branch: null },
].map((s) => ({ ...s, fields: { is_sandboxed: true } }));

async function confirmRecreate(page: Page, shot: string) {
  const posts: string[] = [];
  await page.route("**/api/sessions/*/recreate-container", (r) => {
    posts.push(`${r.request().method()} ${new URL(r.request().url()).pathname}`);
    return r.fulfill({ json: {} });
  });

  await page.locator("[data-testid='sidebar-session-row']").first().click({ button: "right" });
  await page.getByTestId("sidebar-context-menu-more").click();
  const item = page.getByTestId("sidebar-context-menu-recreate-container");
  await expect(item).toBeInViewport({ ratio: 1 });
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${shot}-menu.png`, animations: "disabled" });
  await item.click();

  const dialog = page.getByRole("dialog", { name: "Recreate container" });
  await expect(dialog).toBeInViewport({ ratio: 1 });
  await expect(page.getByTestId("recreate-container-confirm")).toBeInViewport({ ratio: 1 });
  await expect(dialog.getByRole("button", { name: "Cancel" })).toBeInViewport({ ratio: 1 });
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${shot}-dialog.png`, animations: "disabled" });
  expect(posts).toEqual([]);

  await page.getByTestId("recreate-container-confirm").click();
  await expect(dialog).toBeHidden();
  await expect(page.getByText("Restarting Mongols on a new container…")).toBeVisible();
  expect(posts).toEqual(["POST /api/sessions/s-1/recreate-container"]);
}

test.describe("desktop", () => {
  test.use(DESKTOP);

  test("the row menu's More section recreates the container after confirming", async ({ page }) => {
    await installSidebarMocks(page, { sessions: BOXED });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await confirmRecreate(page, "desktop");
  });
});

test.describe("mobile", () => {
  // 375 wide, narrower than the iPhone 13 default.
  test.use({ ...iPhone13, viewport: { width: 375, height: 667 } });

  test("the bottom sheet offers it and the confirmation fits the viewport", async ({ page }) => {
    await installSidebarMocks(page, { sessions: BOXED });
    await page.goto("/");
    await openMobileSidebar(page);
    await confirmRecreate(page, "mobile");
  });
});
