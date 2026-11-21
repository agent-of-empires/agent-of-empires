// The Worktree row's "What's a worktree?" popover (#5910): keyboard, the link's new tab, and phones.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { startWizard, wizard } from "./helpers/wizard";

const WORKTREE_ON = { settings: { worktree: { enabled: true } } };
const icon = (page: Page) => wizard(page).getByRole("button", { name: "What's a worktree?" });
const popover = (page: Page) => page.getByRole("dialog", { name: "What's a worktree?" });
const link = (page: Page) => popover(page).getByRole("link", { name: "Worktree settings ›" });

test.describe("desktop", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("opens from the keyboard, Tab reaches the link, Escape returns to the icon", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    for (const key of ["Enter", "Space"]) {
      await icon(page).focus();
      await page.keyboard.press(key);
      await expect(popover(page)).toBeVisible();
      await expect(popover(page)).toBeFocused();
      await page.keyboard.press("Tab");
      await expect(link(page)).toBeFocused();
      await page.keyboard.press("Escape");
      await expect(popover(page)).toHaveCount(0);
      await expect(icon(page)).toBeFocused();
      await expect(page.getByTestId("session-wizard")).toBeVisible();
    }
  });

  test("the link opens Settings › Worktree in a new tab and the wizard stays open", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    await wizard(page).getByPlaceholder("Auto-generated if empty").fill("keep-me");
    await icon(page).click();
    const [tab] = await Promise.all([page.context().waitForEvent("page"), link(page).click()]);
    await tab.waitForLoadState();
    expect(new URL(tab.url()).pathname).toBe("/settings/worktree");
    expect(await tab.evaluate(() => window.opener)).toBeNull();
    await expect(page.getByTestId("session-wizard")).toBeVisible();
    await expect(wizard(page).getByPlaceholder("Auto-generated if empty")).toHaveValue("keep-me");
  });
});

test.describe("phone", () => {
  test.use({ hasTouch: true, isMobile: true });

  test("a 44px icon opens a popover that fits the screen, without growing the row", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    // startWizard sizes the page for a desktop.
    await page.setViewportSize({ width: 375, height: 812 });
    const box = (await icon(page).boundingBox())!;
    expect(box.width).toBeGreaterThanOrEqual(44);
    expect(box.height).toBeGreaterThanOrEqual(44);
    const rowHeight = async (id: string) => (await wizard(page).getByTestId(id).boundingBox())!.height;
    expect(await rowHeight("wizard-worktree-row")).toBe(await rowHeight("wizard-sandbox-row"));

    await icon(page).tap();
    await expect(popover(page)).toBeInViewport({ ratio: 1 });
    await expect(link(page)).toBeVisible();
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    expect(overflow).toBeLessThanOrEqual(0);
    // A tap outside closes it and leaves the wizard open.
    await wizard(page)
      .getByTestId("wizard-agent-row")
      .tap({ position: { x: 4, y: 4 } });
    await expect(popover(page)).toHaveCount(0);
  });
});
