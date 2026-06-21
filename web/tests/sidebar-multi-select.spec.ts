import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Mocked coverage for sidebar multi-select gestures (#1724, #2312):
//   - Cmd/Ctrl+click toggles a row into the selection without navigating.
//   - Shift+click selects a contiguous range across the rendered rows.
//   - Bulk triage is driven from the right-click context menu (the old
//     BulkActionBar popup was removed in #2312); right-clicking a selected
//     row acts on the whole selection, an unselected row resets to itself.

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
          idle_entered_at: null,
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

const THREE: MockSession[] = [
  { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" },
  { id: "s-2", title: "Goths", project_path: "/tmp/repo-b" },
  { id: "s-3", title: "Persians", project_path: "/tmp/repo-c" },
];

test.describe("Sidebar multi-select (#1724, #2312)", () => {
  test("Cmd/Ctrl+click toggles selection without navigating", async ({ page }) => {
    await mockApis(page, THREE);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);

    // Additive toggle on two rows; the rows show the selected highlight and the
    // route never changes to a /session/ path.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await rows.nth(1).click({ modifiers: ["ControlOrMeta"] });

    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(2);
    expect(page.url()).not.toContain("/session/");

    // Toggling the first row off drops it back out of the selection.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(1);

    // Escape clears the selection.
    await page.keyboard.press("Escape");
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(0);
  });

  test("right-click a selected row bulk-archives the whole selection", async ({ page }) => {
    await mockApis(page, THREE);
    const archived: Array<{ id: string; body: unknown }> = [];
    await page.route("**/api/sessions/*/archive", (r) => {
      const m = r
        .request()
        .url()
        .match(/\/api\/sessions\/([^/]+)\/archive$/);
      archived.push({ id: m?.[1] ?? "?", body: r.request().postDataJSON() });
      return r.fulfill({ json: { id: m?.[1] ?? "?", archived_at: "now" } });
    });

    await page.goto("/");
    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);

    // Anchor on the first row (Cmd+click avoids navigating), then Shift+click
    // the last to select the contiguous range.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await rows.nth(2).click({ modifiers: ["Shift"] });
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(3);

    // Right-clicking a row that is part of the selection opens a bulk menu.
    await rows.nth(1).click({ button: "right" });
    const menu = page.locator("[data-testid='sidebar-context-menu']");
    await expect(menu).toContainText("3 selected");

    const archiveItem = menu.locator("[data-testid='sidebar-context-menu-bulk-archive']");
    await expect(archiveItem).toContainText("Archive 3");
    await archiveItem.click();

    // Serial fan-out hits all three sessions with the archive payload.
    await expect.poll(() => archived.length).toBe(3);
    expect(archived.map((a) => a.id).sort()).toEqual(["s-1", "s-2", "s-3"]);
    for (const a of archived) {
      expect(a.body).toEqual({ archived: true, kill_pane: true });
    }
    // Selection clears once the bulk action completes.
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(0);
  });

  test("right-click an unselected row resets the selection to that row (#2312)", async ({ page }) => {
    await mockApis(page, THREE);
    await page.goto("/");
    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);

    // Build a two-row selection, then right-click a row outside it.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await rows.nth(1).click({ modifiers: ["ControlOrMeta"] });
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(2);

    await rows.nth(2).click({ button: "right" });

    // The old multi-selection is gone; only the right-clicked row is selected
    // and the menu is the single-row menu (no "N selected" header).
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(1);
    const menu = page.locator("[data-testid='sidebar-context-menu']");
    await expect(menu).toBeVisible();
    await expect(menu).not.toContainText("selected");
    await expect(menu.locator("[data-testid='sidebar-context-menu-bulk-archive']")).toHaveCount(0);
  });

  test("plain click then Shift+click selects the range from the navigated row (#2312)", async ({ page }) => {
    await mockApis(page, THREE);
    await page.goto("/");
    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);

    // Plain click navigates to the first session and leaves it as the anchor;
    // no intervening Cmd+click is needed before the range works.
    await rows.nth(0).click();
    await expect.poll(() => page.url()).toContain("/session/s-1");

    await rows.nth(2).click({ modifiers: ["Shift"] });
    await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(3);
  });
});
