import { test, expect } from "./helpers/mockedTest";

const STORAGE_KEY = "aoe-right-collapsed";

async function getStored(page: import("@playwright/test").Page) {
  return page.evaluate((k) => localStorage.getItem(k), STORAGE_KEY);
}

test.describe("Right panel collapsed-state persistence", () => {
  test("desktop with empty storage seeds expanded ('0')", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("0");
  });

  test("mobile with empty storage seeds collapsed ('1')", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("1");
  });

  test("stored '1' overrides desktop viewport default", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.addInitScript((k) => localStorage.setItem(k, "1"), STORAGE_KEY);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("1");
  });

  test("stored '0' overrides mobile viewport default", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.addInitScript((k) => localStorage.setItem(k, "0"), STORAGE_KEY);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("0");
  });

  test("keyboard toggle flips the stored value and survives reload", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("0");

    // ControlOrMeta+Alt+B = right panel toggle (see useKeyboardShortcuts).
    // Focus the body first so the handler reliably receives the event.
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+Alt+B");
    await expect.poll(() => getStored(page)).toBe("1");

    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    expect(await getStored(page)).toBe("1");
  });
});
