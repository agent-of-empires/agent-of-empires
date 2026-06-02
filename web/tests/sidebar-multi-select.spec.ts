import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// Mocked coverage for sidebar multi-select gestures (#1724):
//   - Cmd/Ctrl+click toggles a row into the selection without navigating.
//   - Shift+click selects a contiguous range across the rendered rows.
//   - The bulk bar then fans out one PATCH per selected session.

interface MockSession {
  id: string;
  title: string;
  project_path: string;
}

async function mockApis(page: Page, sessions: MockSession[]) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
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
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "docker/status" ? {} : [] }),
    );
  }
}

const THREE: MockSession[] = [
  { id: "s-1", title: "Mongols", project_path: "/tmp/repo-a" },
  { id: "s-2", title: "Goths", project_path: "/tmp/repo-b" },
  { id: "s-3", title: "Persians", project_path: "/tmp/repo-c" },
];

test.describe("Sidebar multi-select (#1724)", () => {
  test("Cmd/Ctrl+click toggles selection without navigating", async ({
    page,
  }) => {
    await mockApis(page, THREE);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    const rows = page.locator("[data-testid='sidebar-session-row']");
    await expect(rows).toHaveCount(3);

    // Additive toggle on two rows; the bulk bar reflects the count and the
    // route never changes to a /session/ path.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await rows.nth(1).click({ modifiers: ["ControlOrMeta"] });

    const bar = page.locator("[data-testid='sidebar-bulk-bar']");
    await expect(bar).toBeVisible();
    await expect(bar).toContainText("2 selected");
    expect(page.url()).not.toContain("/session/");

    // Toggling the first row off drops it back out of the selection.
    await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
    await expect(bar).toContainText("1 selected");

    // Clear empties the selection and hides the bar.
    await page.locator("[data-testid='sidebar-bulk-clear']").click();
    await expect(bar).toHaveCount(0);
  });

  test("Shift+click range selects every row in between, then bulk archives", async ({
    page,
  }) => {
    await mockApis(page, THREE);
    const archived: Array<{ id: string; body: unknown }> = [];
    await page.route("**/api/sessions/*/archive", (r) => {
      const m = r.request().url().match(/\/api\/sessions\/([^/]+)\/archive$/);
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

    const bar = page.locator("[data-testid='sidebar-bulk-bar']");
    await expect(bar).toContainText("3 selected");

    const archiveBtn = page.locator("[data-testid='sidebar-bulk-archive']");
    await expect(archiveBtn).toContainText("Archive 3");
    await archiveBtn.click();

    // Serial fan-out hits all three sessions with the archive payload.
    await expect.poll(() => archived.length).toBe(3);
    expect(archived.map((a) => a.id).sort()).toEqual(["s-1", "s-2", "s-3"]);
    for (const a of archived) {
      expect(a.body).toEqual({ archived: true, kill_pane: true });
    }
    // Selection clears once the bulk action completes.
    await expect(bar).toHaveCount(0);
  });
});
