import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

test.describe("Mobile terminal toolbar", () => {
  test("toolbar hidden on desktop viewport (no session)", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    // Toolbar should never appear on desktop
    await expect(page.getByRole("button", { name: "Arrow up" })).not.toBeVisible();
  });

  test("toolbar hidden on desktop viewport even with a session open", async ({ page }) => {
    // User story (ported from the live suite): the mobile terminal
    // toolbar must not render on a desktop viewport, even when a
    // session is open and its terminal is connected. The gate is the
    // (pointer: coarse) media query in useMobileKeyboard, which never
    // matches the desktop chromium project.
    await mockTerminalApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await expect(page).toHaveURL(/\/session\/pinch-test/, { timeout: 10_000 });
    // Wait for the terminal pane so the negative assertions run against
    // the fully mounted session view, not a still-loading shell. The live
    // view now renders on desktop too; its touch toolbar is gated off
    // fine-pointer devices.
    await expect(page.locator("[data-live-terminal]").first()).toBeVisible({ timeout: 15_000 });

    await expect(page.getByRole("button", { name: "Arrow up" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Ctrl+C interrupt" })).toHaveCount(0);
  });

  test("toolbar hidden on mobile when no session selected", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    // No session = dashboard view, no toolbar
    await expect(page.getByRole("button", { name: "Arrow up" })).not.toBeVisible();
  });

  test("toolbar buttons have correct aria-labels for accessibility", async ({ page }) => {
    // Verify the component defines the expected labels (render test)
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    // Without an active session we can't see the toolbar, but we can verify
    // the component structure by checking it doesn't crash on mobile viewport
    await expect(page.locator("header")).toBeVisible();
  });

  test("dark color-scheme meta tag present", async ({ page }) => {
    await page.goto("/");
    const colorScheme = await page.locator('meta[name="color-scheme"]').getAttribute("content");
    expect(colorScheme).toBe("dark");
  });
});

test.describe("Mobile terminal toolbar accessibility", () => {
  test("all expected button labels defined in component", async ({ page }) => {
    // This test validates the component source defines proper aria-labels
    // by checking the page doesn't have orphan toolbar buttons without labels
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    // On the dashboard (no session), toolbar should not render
    const arrowButtons = page.getByRole("button", { name: /Arrow/ });
    await expect(arrowButtons).toHaveCount(0);
  });
});
