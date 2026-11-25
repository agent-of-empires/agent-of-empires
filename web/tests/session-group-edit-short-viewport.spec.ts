// Edit group dialog on a short viewport: the open inline list must not push Save or
// Remove from group off-screen, and both must take real clicks.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks } from "./helpers/sidebarMocks";
import { sessionResponse } from "./helpers/sessions";

const ID = "s-1";
const GROUPS = ["work", ...["a", "b", "c", "d", "e", "f", "g"].map((s) => `team-${s}`)];

async function openEditGroup(page: Page) {
  await installSidebarMocks(page, {
    sessions: [{ id: ID, title: "Mongols", project_path: "/tmp/repo", branch: null, group: "work" }],
  });
  await page.route(/\/api\/groups(\?.*)?$/, (r) =>
    r.fulfill({ json: GROUPS.map((path) => ({ path, session_count: 1 })) }),
  );
  const bodies: unknown[] = [];
  await page.route(`**/api/sessions/${ID}/group`, (r) => {
    bodies.push(r.request().postDataJSON());
    return r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "2" },
      json: sessionResponse({ id: ID, title: "Mongols", project_path: "/tmp/repo", group_path: "" }),
    });
  });
  await page.setViewportSize({ width: 800, height: 420 });
  await page.goto("/");
  await page.locator("[data-testid='sidebar-session-row']").click({ button: "right" });
  await page.locator("[data-testid='sidebar-context-menu-more']").click();
  await page.locator("[data-testid='sidebar-context-menu-edit-group']").click();
  // Groups load after the list opens; measure only once they are all rows.
  await expect(page.getByRole("option")).toHaveCount(GROUPS.length);
  return bodies;
}

async function expectInViewport(page: Page, testId: string) {
  const box = await page.getByTestId(testId).boundingBox();
  const viewport = page.viewportSize()!;
  expect(box).not.toBeNull();
  expect(box!.y).toBeGreaterThanOrEqual(0);
  expect(box!.y + box!.height).toBeLessThanOrEqual(viewport.height);
}

test.describe("Edit group dialog on a short viewport", () => {
  test("Save is on screen and clickable with the list open", async ({ page }) => {
    const bodies = await openEditGroup(page);
    await page.getByTestId("session-group-modal-input").fill("team");
    await expect(page.getByRole("option", { name: /team-g/ })).toBeVisible();
    await expectInViewport(page, "session-group-modal-save");
    await page.getByTestId("session-group-modal-save").click();
    await expect(page.getByTestId("session-group-modal")).toBeHidden();
    expect(bodies).toEqual([{ group: "team" }]);
  });

  test("Remove from group is on screen and clickable with the list open", async ({ page }) => {
    const bodies = await openEditGroup(page);
    await expectInViewport(page, "session-group-modal-remove");
    await page.getByTestId("session-group-modal-remove").click();
    await expect(page.getByTestId("session-group-modal")).toBeHidden();
    expect(bodies).toEqual([{ group: "" }]);
  });
});
