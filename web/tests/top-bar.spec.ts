import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

test.describe("Top bar", () => {
  test("renders sidebar toggle, brand, palette pill, and overflow", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Toggle sidebar" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Open command palette" }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "More options" })).toBeVisible();
  });

  test("overflow menu opens on click and exposes help actions", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await expect(page.getByRole("menuitem", { name: "Help" })).toBeVisible();
    await expect(page.getByRole("menuitem", { name: "About" })).toBeVisible();
  });

  test("overflow menu closes on outside click", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await expect(page.getByRole("menuitem", { name: "Help" })).toBeVisible();
    await page.mouse.click(300, 300);
    await expect(page.getByRole("menuitem", { name: "Help" })).not.toBeVisible();
  });

  test("overflow Help opens help overlay", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Help" }).click();
    await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
    // A sample binding row, proving the shortcuts list rendered and not
    // just the heading (ported from the live modal-help story).
    await expect(page.getByText(/Toggle this help/i)).toBeVisible();
  });

  test("overflow About opens About modal with links", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).toBeVisible();
    await expect(page.getByRole("link", { name: /agent-of-empires\.com/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /github\.com\/agent-of-empires/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /@agentofempires/i })).toBeVisible();
  });

  test("About modal closes via the X", async ({ page }) => {
    // Ported from the live modal-about story: the X (aria-label
    // "Close") in the dialog header unmounts the modal.
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText("Agent of Empires")).toBeVisible();
    await dialog.getByRole("button", { name: "Close" }).click();
    await expect(dialog).toBeHidden();
  });

  test("Go to dashboard returns to / from a session view", async ({ page }) => {
    // Ported from the live topbar-go-to-dashboard story: from a session
    // route, the brand button navigates home.
    await mockTerminalApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/pinch-test");
    await expect(page).toHaveURL("/session/pinch-test");

    await page.getByRole("button", { name: "Go to dashboard" }).click();
    await expect(page).toHaveURL("/");
  });

  test("About modal closes on Escape", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).not.toBeVisible();
  });

  test("offline indicator shows when API unreachable", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("offline")).toBeVisible();
  });

  test("mobile: palette trigger collapses to icon", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    // The icon-only variant is still accessible via the same aria-label
    await expect(page.getByRole("button", { name: "Open command palette" }).first()).toBeVisible();
  });
});
