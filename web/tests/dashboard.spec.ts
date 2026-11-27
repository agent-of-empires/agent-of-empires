import { test, expect, pressShortcut } from "./helpers/mockedTest";
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
    // A saved project needs served-profile authority and an online session list.
    await page.route("**/api/about", (r) => r.fulfill({ json: { profile: "default" } }));
    await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: [], workspace_ordering: [] },
      }),
    );
    await page.route("**/api/profiles", (r) => r.fulfill({ json: [{ name: "default", is_default: true }] }));
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
    const addBtn = page.getByTestId("sidebar-new-session");
    await expect(addBtn).toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).not.toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).toBeVisible();
  });
});

test.describe("Create session from home screen", () => {
  test("'Clone URL' pane opens wizard on Clone tab", async ({ page }) => {
    await page.route("**/api/profiles", (r) => r.fulfill({ json: [{ name: "default", is_default: true }] }));
    await page.route("**/api/projects*", (r) => r.fulfill({ json: [] }));
    await page.goto("/");
    await page.getByText("Clone URL").click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // Should be on the Clone tab, showing the URL input
    await expect(page.getByPlaceholder("https://github.com/user/repo.git")).toBeVisible();
  });

  test("opens with keyboard shortcut n", async ({ page }) => {
    await page.route("**/api/about", (r) => r.fulfill({ json: { profile: "default" } }));
    // Answer the session poll, or its failure marks the server unreachable and disables New session before `n`.
    await page.route("**/api/sessions", (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: [], workspace_ordering: [] },
      }),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await pressShortcut(page, "n");
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
    await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: [], workspace_ordering: [] },
      }),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await page.getByTestId("sidebar-new-session").first().click();
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
    await pressShortcut(page, "s");
    await expect(page.getByRole("banner").getByRole("button", { name: "Back", exact: true })).toBeVisible();
  });
});

test.describe("Keyboard shortcuts", () => {
  test("escape closes help overlay", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await pressShortcut(page, "?");
    await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("heading", { name: "Help" })).not.toBeVisible();
  });
});

test.describe("Phone session stack", () => {
  const phoneSessions = [
    sessionResponse({ id: "alpha", title: "Alpha", project_path: "/repo/shop", branch: "feat/cart" }),
    sessionResponse({ id: "beta", title: "Beta", project_path: "/repo/api", status: "Waiting" }),
    sessionResponse({ id: "gamma", title: "docs", project_path: "/repo/docs", status: "Idle" }),
  ];

  test.beforeEach(async ({ page }) => {
    await page.route(/\/api\/sessions(\?.*)?$/, (route) =>
      route.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: phoneSessions, workspace_ordering: [] },
      }),
    );
    await page.setViewportSize({ width: 375, height: 812 });
  });

  test("the landing page is home; ☰ opens the session drawer, whose Home goes back to it", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    const drawer = page.getByTestId("sidebar-new-session-pill");
    await expect(drawer).toBeHidden();
    // Beta waits, so ☰ carries the count.
    const menu = page.getByTestId("topbar-menu");
    await expect(menu).toHaveAccessibleName("Show sessions, 1 needs you");

    await menu.click();
    await expect(drawer).toBeInViewport();
    await expect(page.getByTestId("sidebar-row-status-line").first()).toBeVisible();
    await page.getByTestId("sidebar-session-row").filter({ hasText: "Alpha" }).click();
    await expect(page).toHaveURL(/\/session\/alpha$/);
    const pill = page.getByTestId("session-title-pill");
    await expect(pill).toContainText("Alpha");
    await expect(pill).toContainText("shop");
    await expect(drawer).toBeHidden();

    await menu.click();
    await expect(drawer).toBeInViewport();
    await page.getByTestId("sidebar-home").click();
    await expect(page).toHaveURL(/\/$/);
    await expect(drawer).toBeHidden();
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
  });

  test("tapping the title pill opens the Session's menu; its folder drops out where it repeats the title", async ({
    page,
  }) => {
    await page.goto("/session/beta");
    const pill = page.getByTestId("session-title-pill");
    await expect(pill).toContainText("api");
    await pill.click();
    const menu = page.getByTestId("sidebar-context-menu");
    await expect(menu).toBeVisible();
    await expect(menu.getByTestId("sidebar-context-menu-rename")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();

    await page.goto("/session/gamma");
    await expect(page.getByTestId("session-title-status")).toHaveText("Idle");
  });

  test("the header's ⋯ holds the open Session's views, then its actions", async ({ page }) => {
    await page.goto("/session/alpha");
    await page.getByRole("button", { name: "More options" }).click();
    const items = page.getByRole("menuitem");
    await expect(items).toHaveText([
      /Copy branch\s*feat\/cart/,
      "Pin",
      "Snooze…",
      "Archive",
      "Stop",
      "Move to Trash",
      "Help",
      "Symbol legend",
      "Show tutorial",
      "Tips",
      "About",
    ]);
    await expect(page.getByRole("separator")).toHaveCount(2);
  });
});
