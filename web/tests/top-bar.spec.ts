import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";

test.describe("Top bar", () => {
  test("renders sidebar toggle, brand, palette pill, overflow, and the offline badge", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.getByRole("button", { name: "Toggle sidebar" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Open command palette" }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "More options" })).toBeVisible();
    // No backend behind vite preview, so the connectivity badge shows.
    await expect(page.getByText("offline")).toBeVisible();
  });

  test("overflow About and Help open their modals; Escape and the X close About", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).toBeVisible();
    await expect(page.getByRole("link", { name: /agent-of-empires\.com/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /github\.com\/agent-of-empires/i })).toBeVisible();
    await expect(page.getByRole("link", { name: /@agentofempires/i })).toBeVisible();

    // Escape closes it; reopened, the header X closes it too.
    await page.keyboard.press("Escape");
    await expect(page.getByRole("heading", { name: "Agent of Empires" })).not.toBeVisible();
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "About" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("Agent of Empires")).toBeVisible();
    await dialog.getByRole("button", { name: "Close" }).click();
    await expect(dialog).toBeHidden();

    // Help opens from the same overflow menu.
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Help" }).click();
    await expect(page.getByRole("heading", { name: "Help" })).toBeVisible();
    // A sample binding row, proving the shortcuts list rendered and not
    // just the heading (ported from the live modal-help story).
    await expect(page.getByText(/Toggle this help/i)).toBeVisible();
  });

  test("Go to dashboard returns to / from a session view", async ({ page }) => {
    await mockTerminalApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/session/pinch-test");
    await expect(page).toHaveURL((url) => url.pathname === "/session/pinch-test");
    await expect(page.locator("[data-live-terminal]").first()).toBeVisible();

    await page.getByRole("button", { name: "Go to dashboard" }).click();
    await expect(page).toHaveURL("/");
  });
});
