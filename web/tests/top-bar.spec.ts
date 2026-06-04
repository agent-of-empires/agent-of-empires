import { test, expect } from "./helpers/mockedTest";

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
