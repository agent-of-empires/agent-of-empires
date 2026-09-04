import { test, expect } from "./helpers/mockedTest";
import { Page, Locator } from "@playwright/test";

// Regression for #1601: the session-row and repo-group context menus
// must clamp inside the viewport when opened near the bottom or right
// edge, so every menu item stays reachable.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
}

async function mockApis(page: Page, sessions: MockSession[]) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: sessions.map((s) => ({
          id: s.id,
          title: s.title,
          project_path: s.project_path,
          group_path: s.project_path,
          tool: "claude",
          status: "Idle",
          yolo_mode: false,
          created_at: new Date().toISOString(),
          last_accessed_at: null,
          last_error: null,
          branch: null,
          main_repo_path: null,
          is_sandboxed: false,
          has_terminal: true,
          profile: "default",
          workspace_repos: [],
        })),
        workspace_ordering: [],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

async function menuFitsViewport(page: Page, locator: string) {
  // Web fonts and icons can grow the menu after first paint, so the
  // component reclamps via ResizeObserver. Wait for fonts to settle
  // before sampling the bounding box so this test does not race the
  // late layout. See #1601.
  await page.evaluate(() => document.fonts?.ready);
  const viewport = page.viewportSize();
  expect(viewport).not.toBeNull();
  await expect
    .poll(
      async () => {
        const box = await page.locator(locator).boundingBox();
        if (!box) return null;
        return (
          box.x >= 0 && box.y >= 0 && box.x + box.width <= viewport!.width && box.y + box.height <= viewport!.height
        );
      },
      { timeout: 5_000 },
    )
    .toBe(true);
}

async function expectDvhCap(menu: Locator) {
  // Regression for #2870: the cap must use the dynamic viewport height.
  // On iOS Safari `100vh` overshoots the visible viewport (dynamic
  // toolbar), so a menu taller than the visible area never exceeds its
  // own max-height and `overflow-y-auto` never engages, leaving Delete
  // behind the toolbar with no way to scroll to it. `dvh` shrinks with
  // the toolbar and matches the `window.innerHeight` the clamp measures.
  const maxHeight = await menu.evaluate((el) => el.style.maxHeight);
  expect(maxHeight).toContain("dvh");
  await expect(menu).toHaveClass(/overflow-y-auto/);
}

test.describe("Sidebar context-menu viewport clamp (#1601)", () => {
  test("right-click on the bottom session row keeps the menu inside the viewport", async ({ page }) => {
    await mockApis(page, [
      { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" },
      { id: "s-2", title: "Goths", project_path: "/tmp/repo-b" },
      { id: "s-3", title: "Persians", project_path: "/tmp/repo-c" },
    ]);
    await page.setViewportSize({ width: 900, height: 360 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);
    const lastRow = rows.last();
    await lastRow.scrollIntoViewIfNeeded();
    await lastRow.click({ button: "right" });

    const menu = page.locator("[data-testid='sidebar-context-menu']");
    await expect(menu).toBeVisible();
    await menuFitsViewport(page, "[data-testid='sidebar-context-menu']");
    await expectDvhCap(menu);
  });

  test("right-click on a repo group header near the bottom clamps the menu", async ({ page }) => {
    await mockApis(page, [
      { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" },
      { id: "s-2", title: "Goths", project_path: "/tmp/repo-b" },
      { id: "s-3", title: "Persians", project_path: "/tmp/repo-c" },
    ]);
    await page.setViewportSize({ width: 900, height: 360 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const headers = page.locator("[data-testid='sidebar-group-header']");
    await expect(headers).toHaveCount(3);
    const lastHeader = headers.last();
    await lastHeader.scrollIntoViewIfNeeded();
    await lastHeader.click({ button: "right" });

    const menu = page.locator("[data-testid='sidebar-group-context-menu']");
    await expect(menu).toBeVisible();
    await menuFitsViewport(page, "[data-testid='sidebar-group-context-menu']");
    await expectDvhCap(menu);
  });
});
