import { test, expect } from "./helpers/mockedTest";
import { sessionResponse } from "./helpers/sessions";

const NEW_SESSION_PANE_NAME = /New session Pick a project, then launch a new session/i;

test.describe("Dashboard layout", () => {
  test("shows branded home screen with action panes", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("empires", { exact: false })).toBeVisible();
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page.getByText("Clone URL")).toBeVisible();
    await expect(page.getByText("Docs")).toBeVisible();
  });
});

test.describe("Sidebar", () => {
  test("sidebar Projects section lists a no-session saved project with an add button", async ({ page }) => {
    // The dedicated Projects section (#2212) replaced the /projects page: a
    // saved (non-pinned) project with no live session renders as a row in the
    // sidebar, alongside an add-project button. Stub /api/sessions so the app
    // reports online, otherwise the add button stays hidden.
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    await page.route("**/api/projects*", (r) =>
      r.fulfill({ json: [{ name: "saved-repo", path: "/work/saved-repo", scope: "global", pinned: false }] }),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const section = page.getByTestId("sidebar-projects-section");
    await expect(section).toBeVisible();
    await expect(section.getByText("saved-repo", { exact: true })).toBeVisible();
    await expect(page.getByTestId("sidebar-projects-add")).toBeVisible();
  });

  test("sidebar can be toggled closed and open on desktop", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    const addBtn = page.getByLabel("New project session");
    await expect(addBtn).toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).not.toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).toBeVisible();
  });
});

test.describe("Create session from home screen", () => {
  test("'Clone URL' pane opens wizard on Clone tab", async ({ page }) => {
    await page.goto("/");
    await page.getByText("Clone URL").click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // Should be on the Clone tab, showing the URL input
    await expect(page.getByPlaceholder("https://github.com/user/repo.git")).toBeVisible();
  });

  test("opens with keyboard shortcut n", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("n");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  });

  test("wizard closes on escape", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: NEW_SESSION_PANE_NAME }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("heading", { name: "New session" })).not.toBeVisible();
  });

  test("sidebar New session opens wizard and the header X closes it", async ({ page }) => {
    // Ported from the live wizard-open-close story. Stub /api/sessions
    // so useSessions reports the server reachable; otherwise the
    // offline-state UI disables the sidebar "New session" trigger.
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await page.getByLabel("New project session").first().click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();

    await page.getByRole("button", { name: "Close" }).click();
    await expect(page.getByRole("heading", { name: "New session" })).not.toBeVisible();
  });

  test("wizard closes on backdrop click", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: NEW_SESSION_PANE_NAME }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // Click the backdrop (top-left corner, outside the modal)
    await page.mouse.click(10, 10);
    await expect(page.getByRole("heading", { name: "New session" })).not.toBeVisible();
  });
});

test.describe("Settings", () => {
  test("settings opens with keyboard shortcut s", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("s");
    await expect(page.getByRole("button", { name: /Back/i })).toBeVisible();
  });
});

test.describe("Keyboard shortcuts", () => {
  test("escape closes help overlay", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.evaluate(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    });
    await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("heading", { name: "Help" })).not.toBeVisible();
  });
});

test.describe("Mobile responsive", () => {
  test("mobile home screen's Show sessions button opens the sidebar", async ({ page }) => {
    // Dashboard.tsx's own `md:hidden` trigger, not TopBar's "Toggle sidebar":
    // the two are separate elements and nothing else in web/ drives this one.
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByText("Show sessions").click();
    await expect(page.getByLabel("New project session")).toBeInViewport();
  });

  test("mobile home offers the five most recent sessions while desktop keeps them hidden", async ({ page }) => {
    const sessions = [
      ["old", "2026-01-02T00:00:00Z"],
      ["new", "2026-01-07T00:00:00Z"],
      ["middle", "2026-01-04T00:00:00Z"],
      ["four", "2026-01-05T00:00:00Z"],
      ["five", "2026-01-06T00:00:00Z"],
      ["six", "2026-01-03T00:00:00Z"],
      ["trash", "2026-01-08T00:00:00Z"],
    ].map(([id, last_accessed_at]) =>
      sessionResponse({
        id: id!,
        title: `Session ${id}`,
        project_path: `/repo/${id}`,
        artifact_dir: `/tmp/${id}`,
        group_path: "",
        dormant: false,
        created_at: "2026-01-01T00:00:00Z",
        last_accessed_at,
        main_repo_path: `/repo/${id}`,
        scratch: false,
        favorited: false,
        has_managed_worktree: false,
        cleanup_defaults: {},
        remote_owner: null,
        notify_on_waiting: null,
        notify_on_idle: null,
        notify_on_error: null,
        trashed_at: id === "trash" ? "2026-01-08T00:00:00Z" : null,
      }),
    );
    await page.route("**/api/sessions", (route) => route.fulfill({ json: { sessions, workspace_ordering: [] } }));

    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    const recent = page.getByLabel("Recent sessions");
    await expect(recent).toBeVisible();
    await expect(recent.getByRole("button")).toHaveCount(5);
    await expect(recent.getByRole("button").allTextContents()).resolves.toEqual([
      expect.stringContaining("Session new"),
      expect.stringContaining("Session five"),
      expect.stringContaining("Session four"),
      expect.stringContaining("Session middle"),
      expect.stringContaining("Session six"),
    ]);

    await page.setViewportSize({ width: 1280, height: 720 });
    await expect(recent).not.toBeAttached();

    await page.setViewportSize({ width: 375, height: 812 });
    await expect(recent).toBeVisible();
    await recent.getByRole("button", { name: /Session new/ }).click();
    await expect(page).toHaveURL(/\/session\/new$/);
  });

  test("sidebar closes via toggle on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.getByLabel("New project session")).toBeInViewport();
    // Toggle the sidebar closed again
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.getByLabel("New project session")).not.toBeInViewport();
  });
});
