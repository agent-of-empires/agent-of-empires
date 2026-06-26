import { test, expect } from "./helpers/mockedTest";

const LAYOUT_KEY = "aoe-pane-layout";
const LEGACY_KEY = "aoe-right-collapsed";

// The stored layout is `{ diff: { open, dock }, ... }`; these tests only assert
// the open flags, so flatten each pane to its boolean `open`.
async function getLayout(page: import("@playwright/test").Page) {
  const raw = await page.evaluate((k) => localStorage.getItem(k), LAYOUT_KEY);
  if (!raw) return null;
  const parsed = JSON.parse(raw) as Record<string, { open: boolean }>;
  return Object.fromEntries(Object.entries(parsed).map(([k, v]) => [k, v.open]));
}

test.describe("Right dock pane-layout persistence", () => {
  test("desktop with empty storage seeds both panes open", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: true, terminal: true });
  });

  test("mobile with empty storage seeds both panes closed", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: false, terminal: false });
  });

  test("migrates the legacy collapsed flag '1' to both panes closed", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.addInitScript((k) => localStorage.setItem(k, "1"), LEGACY_KEY);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: false, terminal: false });
  });

  test("stored layout overrides the mobile viewport default", async ({ page }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.addInitScript(
      (k) => localStorage.setItem(k, JSON.stringify({ diff: true, terminal: true })),
      LAYOUT_KEY,
    );
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: true, terminal: true });
  });

  test("keyboard toggle flips the diff pane and survives reload", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: true, terminal: true });

    // Shift+D toggles the diff pane specifically (Ctrl+Alt+B now collapses the
    // whole dock). Focus the body first so the handler receives the event.
    await page.locator("body").click();
    await page.keyboard.press("Shift+D");
    await expect.poll(() => getLayout(page)).toEqual({ diff: false, terminal: true });

    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: false, terminal: true });
  });
});
