// The Worktree row's "What's a worktree?" popover (#5910): keyboard, the Settings link, and phones.

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

  test("Shift+Tab out of the open popover closes it and leaves focus where Tab put it", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    await icon(page).focus();
    await page.keyboard.press("Enter");
    await expect(popover(page)).toBeFocused();
    await page.keyboard.press("Shift+Tab");
    await expect(popover(page)).toHaveCount(0);
    await expect(page.getByTestId("session-wizard")).toBeVisible();
    // Focus stays on the wizard control Shift+Tab reached; the popover doesn't take it back.
    expect(await page.evaluate(() => !!document.activeElement?.closest('[data-testid="session-wizard"]'))).toBe(true);
  });

  const focusInWizard = (page: Page) =>
    page.evaluate(() => !!document.activeElement?.closest('[data-testid="session-wizard"]'));

  // Opened first, then resized: a narrower window closes it, a shorter one keeps it open (re-clamped, see below).
  // Closing under focus hands it back to the icon, so the next Tab goes on through the wizard.
  test("an open popover closes when the window narrows, and stays open when only the height changes", async ({
    page,
  }) => {
    await startWizard(page, WORKTREE_ON);
    await icon(page).focus();
    await page.keyboard.press("Enter");
    await page.keyboard.press("Tab");
    await expect(link(page)).toBeFocused();
    await page.setViewportSize({ width: 1280, height: 600 });
    await expect(popover(page)).toBeVisible();
    await expect(link(page)).toBeFocused();
    await page.setViewportSize({ width: 375, height: 600 });
    await expect(popover(page)).toHaveCount(0);
    await expect(icon(page)).toBeFocused();
    await page.keyboard.press("Tab");
    expect(await focusInWizard(page)).toBe(true);
  });

  test("a scroll under the open popover closes it and hands focus back to the icon", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    await icon(page).focus();
    await page.keyboard.press("Enter");
    await page.keyboard.press("Tab");
    await expect(link(page)).toBeFocused();
    // The mocked wizard's fields fit, so a spacer gives its list something to scroll.
    const scrolled = await page.evaluate(() => {
      const list = document.querySelector<HTMLElement>("[data-wizard-scroll]");
      if (!list) return false;
      list.append(Object.assign(document.createElement("div"), { style: "height: 2000px" }));
      list.scrollTop = 40;
      return list.scrollTop > 0;
    });
    expect(scrolled, "the wizard's list didn't scroll").toBe(true);
    await expect(popover(page)).toHaveCount(0);
    await expect(icon(page)).toBeFocused();
    await page.keyboard.press("Tab");
    expect(await focusInWizard(page)).toBe(true);
  });

  test("a height-only shrink past the open popover's bottom edge moves it back inside the window", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    await icon(page).click();
    const bottom = () => popover(page).evaluate((el) => el.getBoundingClientRect().bottom);
    const before = await bottom();
    const height = Math.floor(before) - 100;
    await page.setViewportSize({ width: 1280, height });
    await expect(popover(page)).toBeVisible();
    await expect.poll(bottom).toBeLessThanOrEqual(height);
    await expect(link(page)).toBeInViewport({ ratio: 1 });
  });

  test("the link closes the wizard and opens Settings › Worktree without reloading the page", async ({ page }) => {
    await startWizard(page, WORKTREE_ON);
    // One Worktree setting, so the tab's own row proves where it landed.
    const setting = {
      section: "worktree",
      field: "path_template",
      category: "Worktree",
      label: "Path template",
      description: "",
      widget: { kind: "text" },
      web_write: { policy: "allow" },
      profile_overridable: true,
      validation: {},
      advanced: false,
    };
    await page.route("**/api/settings/schema", (r) => r.fulfill({ json: [setting] }));
    await page.evaluate(() => ((window as unknown as { sameDocument: boolean }).sameDocument = true));
    await icon(page).click();
    await link(page).click();
    await expect(page.getByTestId("session-wizard")).toHaveCount(0);
    await expect(page).toHaveURL(/\/settings\/worktree$/);
    await expect(page.getByText("Path template", { exact: true })).toBeVisible();
    expect(await page.evaluate(() => (window as unknown as { sameDocument?: boolean }).sameDocument)).toBe(true);
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
