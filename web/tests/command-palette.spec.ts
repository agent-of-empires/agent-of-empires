import { test, expect } from "./helpers/mockedTest";

test.describe("Command palette", () => {
  test("opens with Ctrl+K", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    const search = page.getByPlaceholder("Search actions, sessions, settings…");
    await expect(search).toBeVisible();
    // The search input must receive focus on open so the user can type
    // immediately (ported from the live shortcut-palette story).
    await expect(search).toBeFocused();
  });

  test("opens with Meta+K", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("Meta+k");
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
  });

  test("opens via header pill click", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "Open command palette" }).first().click();
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
  });

  test("closes on Escape", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).not.toBeVisible();
  });

  test("closes on backdrop click", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
    await page.locator('[data-testid="command-palette-backdrop"]').click({
      position: { x: 10, y: 10 },
    });
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).not.toBeVisible();
  });

  test("shows initial action groups", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await expect(page.getByRole("option", { name: /New session/i })).toBeVisible();
    await expect(page.getByRole("option", { name: /Go to dashboard/i })).toBeVisible();
    await expect(page.getByRole("option", { name: /Open settings/i })).toBeVisible();
  });

  test("typing filters results", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByPlaceholder("Search actions, sessions, settings…").fill("settings");
    await expect(page.getByRole("option", { name: /Open settings/i })).toBeVisible();
    await expect(page.getByRole("option", { name: /New session/i })).not.toBeVisible();
  });

  test("empty state on no matches", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByPlaceholder("Search actions, sessions, settings…").fill("zzzxxqqq");
    await expect(page.getByText("No matches")).toBeVisible();
  });

  test("enter executes selected action", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByPlaceholder("Search actions, sessions, settings…").fill("new session");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("Enter");
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
  });

  test("opens from within a focused input", async ({ page }) => {
    // Stub /api/sessions so useSessions reports the server as reachable;
    // otherwise useServerDown disables the sidebar "New session" button
    // (introduced with the offline-state UI) and the click below would
    // time out waiting for the button to become enabled. The dashboard
    // offline-indicator test in dashboard.spec.ts exercises the
    // opposite case (no stub → offline UI surfaces).
    await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByLabel("New session").first().click();
    await expect(page.getByRole("heading", { name: "New session" })).toBeVisible();
    await page.getByPlaceholder("Type to filter...").click();
    await page.keyboard.press("ControlOrMeta+k");
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
  });

  test("About action opens About modal", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+k");
    await page.getByPlaceholder("Search actions, sessions, settings…").fill("About Agent");
    await page.keyboard.press("Enter");
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).toBeVisible();
  });

  test("mobile: palette icon button opens palette", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await page.getByRole("button", { name: "Open command palette" }).first().click();
    await expect(page.getByPlaceholder("Search actions, sessions, settings…")).toBeVisible();
  });
});
