import { test, expect } from "./helpers/mockedTest";

const V1_KEY = "aoe-pane-layout"; // pre-#2437 per-pane {open, dock} shape
const V2_KEY = "aoe-pane-layout-v2";
const LEGACY_KEY = "aoe-right-collapsed";
const SESSION = "pinch-test";

// The v2 layout is per-session tab groups. These tests assert only whether the
// diff / terminal kinds are open for the active session, so flatten the active
// session's tabs (falling back to the template before it is seeded) to booleans.
async function getLayout(page: import("@playwright/test").Page) {
  const raw = await page.evaluate((k) => localStorage.getItem(k), V2_KEY);
  if (!raw) return null;
  const store = JSON.parse(raw) as {
    template?: { right?: { tabs: string[] }[]; bottom?: { tabs: string[] }[] };
    sessions?: Record<string, { right?: { tabs: string[] }[]; bottom?: { tabs: string[] }[] }>;
  };
  const layout = store.sessions?.[SESSION] ?? store.template;
  if (!layout) return null;
  const tabs = [...(layout.right?.[0]?.tabs ?? []), ...(layout.bottom?.[0]?.tabs ?? [])];
  return { diff: tabs.includes("diff"), terminal: tabs.some((t) => t.startsWith("terminal:")) };
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
    await page.addInitScript((k) => localStorage.setItem(k, JSON.stringify({ diff: true, terminal: true })), V1_KEY);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    expect(await getLayout(page)).toEqual({ diff: true, terminal: true });
  });

  test("keyboard toggle flips the diff pane and survives reload", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    // Per-session layout: toggles need an active session, so open one directly.
    await page.goto(`/session/${SESSION}`);
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
