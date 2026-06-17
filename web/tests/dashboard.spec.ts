import { test, expect } from "./helpers/mockedTest";

const NEW_SESSION_PANE_NAME = /New session Pick a project, then launch a new session/i;

test.describe("Dashboard layout", () => {
  test("loads and shows header", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
  });

  test("shows branded home screen with logo text", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("empires", { exact: false })).toBeVisible();
  });

  test("shows branded home screen with action panes", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("empires", { exact: false })).toBeVisible();
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
    await expect(page.getByText("Clone URL")).toBeVisible();
    await expect(page.getByText("Docs")).toBeVisible();
  });

  test("shows offline indicator when API unreachable", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("offline")).toBeVisible();
  });
});

test.describe("Sidebar", () => {
  test("sidebar visible on desktop by default", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.getByLabel("New session")).toBeVisible();
  });

  test("sidebar toggle button exists", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Toggle sidebar" })).toBeVisible();
  });

  test("sidebar Projects button opens the Projects view", async ({ page }) => {
    // Ported from the live projects-open story: the sidebar footer's
    // Projects button navigates to /projects and ProjectsView mounts.
    await page.route("**/api/projects*", (r) => r.fulfill({ json: [] }));
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    await page.getByRole("button", { name: "Projects", exact: true }).click();

    await expect(page).toHaveURL(/\/projects/);
    await expect(page.getByRole("heading", { name: "Projects", exact: true })).toBeVisible();
    await expect(page.getByText(/Saved repositories you can multi-select/i)).toBeVisible();
  });

  test("sidebar can be toggled closed and open on desktop", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    const addBtn = page.getByLabel("New session");
    await expect(addBtn).toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).not.toBeVisible();

    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(addBtn).toBeVisible();
  });
});

test.describe("Create session from home screen", () => {
  test("'New session' pane opens session wizard", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: NEW_SESSION_PANE_NAME }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  });

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

  test("wizard closes on the close button", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: NEW_SESSION_PANE_NAME }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    // The single-screen wizard (#2210) has no Back/Cancel footer; it closes
    // via the header close button (or Escape, covered below).
    await page.getByTestId("session-wizard").getByRole("button", { name: "Close" }).click();
    await expect(page.getByRole("heading", { name: "New session" })).not.toBeVisible();
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

    await page.getByLabel("New session").first().click();
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
  test("settings gear button visible", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Settings" })).toBeVisible();
  });

  test("settings opens on click", async ({ page }) => {
    await page.goto("/");
    await page.getByRole("button", { name: "Settings" }).click();
    await expect(page.getByRole("button", { name: /Back/i })).toBeVisible();
  });

  test("settings opens with keyboard shortcut s", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("s");
    await expect(page.getByRole("button", { name: /Back/i })).toBeVisible();
  });
});

test.describe("Keyboard shortcuts", () => {
  test("D toggles diff pane (no-op when no session, no crash)", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    // Should not crash even with no session selected
    await page.keyboard.press("Shift+d");
    await expect(page.getByText("empires", { exact: false })).toBeVisible();
  });

  test("? opens help overlay", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.evaluate(() => {
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    });
    await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
  });

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
  test("sidebar closed by default on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    // Sidebar is translated off-screen on mobile (not display:none), so
    // use toBeInViewport rather than toBeVisible.
    await expect(page.getByLabel("New session")).not.toBeInViewport();
    // Home screen content visible
    await expect(page.getByText("empires", { exact: false })).toBeVisible();
  });

  test("mobile home screen shows sidebar toggle between title and panes", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.getByText("Show sessions")).toBeVisible();
    await expect(page.getByRole("button", { name: NEW_SESSION_PANE_NAME })).toBeVisible();
  });

  test("hamburger opens sidebar overlay on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.getByLabel("New session")).toBeInViewport();
  });

  test("sidebar closes via toggle on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.getByLabel("New session")).toBeInViewport();
    // Toggle the sidebar closed again
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(page.getByLabel("New session")).not.toBeInViewport();
  });

  test("settings gear accessible on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Settings" })).toBeVisible();
  });

  test("create modal works on mobile", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByRole("button", { name: NEW_SESSION_PANE_NAME }).click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  });
});

test.describe("Design system", () => {
  test("uses dark surface background", async ({ page }) => {
    await page.goto("/");
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    // surface-900 = #1c1c1f = rgb(28, 28, 31)
    expect(bg).toContain("28");
    expect(bg).not.toBe("rgb(255, 255, 255)");
  });

  test("loads Geist Sans body font", async ({ page }) => {
    await page.goto("/");
    const fonts = await page.evaluate(() => getComputedStyle(document.body).fontFamily);
    expect(fonts.toLowerCase()).toContain("geist");
  });

  test("focus-visible ring appears on keyboard navigation", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    // Tab to the first button
    await page.keyboard.press("Tab");
    const outline = await page.evaluate(() => {
      const el = document.activeElement;
      return el ? getComputedStyle(el).outlineColor : "";
    });
    // Should have a brand-colored outline
    expect(outline).not.toBe("");
  });
});
