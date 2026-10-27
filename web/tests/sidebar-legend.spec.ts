import { test, expect } from "./helpers/mockedTest";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";
import { openMobileSidebar } from "./helpers/sidebar";

const waitingFirst = () =>
  threeSessionsInOneRepo().map((session, index) =>
    index === 1 ? { ...session, fields: { status: "Waiting" } } : session,
  );

test.describe("sidebar legend on desktop", () => {
  test.beforeEach(async ({ page }) => {
    await installSidebarMocks(page, { sessions: waitingFirst() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
  });

  test("? opens the legend as a popover; Escape, a second press and an outside click close it", async ({ page }) => {
    const toggle = page.getByRole("button", { name: "What do these symbols mean?" });
    const legend = page.getByTestId("sidebar-legend");

    await toggle.click();
    await expect(legend).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    // Every status row, generated from the glyph tables, plus the other marks.
    await expect(legend.getByTestId("sidebar-legend-status")).toHaveCount(10);
    for (const text of ["Waiting · the Agent needs your input", "Dormant", "Unread", "Pinned project", "Group dot"]) {
      await expect(legend).toContainText(text);
    }
    await expect(legend).not.toContainText("Worker");

    await page.keyboard.press("Escape");
    await expect(legend).toBeHidden();
    await expect(toggle).toBeFocused();

    await toggle.click();
    await expect(legend).toBeVisible();
    await toggle.click();
    await expect(legend).toBeHidden();

    await toggle.click();
    await expect(legend).toBeVisible();
    // The outside-click listener attaches a frame after opening, as for every context menu.
    await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    await page.mouse.click(1200, 680);
    await expect(legend).toBeHidden();

    // Compact mode keeps "?", where the glyph is the only signal left on a row.
    await page.getByRole("button", { name: "Compact sidebar" }).click();
    await toggle.click();
    await expect(legend).toBeVisible();
  });

  test("a keyboard-focused row is described by its glyph's tooltip, and the row keeps focus", async ({ page }) => {
    const rows = page.getByTestId("sidebar-session-row");
    await rows.nth(0).focus();
    await page.keyboard.press("ArrowDown");
    await expect(rows.nth(1)).toBeFocused();

    const tip = page.getByRole("tooltip");
    await expect(tip).toContainText("Waiting: the Agent needs your input");
    await expect(tip).toContainText("Press Enter to open");
    await expect(rows.nth(1)).toHaveAttribute("aria-describedby", (await tip.getAttribute("id")) ?? "missing");

    // The next row is described by its own glyph.
    await page.keyboard.press("ArrowDown");
    await expect(tip).not.toContainText("Waiting");
    await page.keyboard.press("ArrowUp");
    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
  });

  test("a keyboard-focused Group header is described by its dot's tooltip", async ({ page }) => {
    await page.getByTestId("sidebar-session-row").first().focus();
    await page.keyboard.press("ArrowUp");
    const header = page.getByTestId("sidebar-group-header").first().getByRole("button").first();
    await expect(header).toBeFocused();
    const tip = page.getByRole("tooltip");
    await expect(tip).toContainText("Active: a Session is Running");
    await expect(header).toHaveAttribute("aria-describedby", (await tip.getAttribute("id")) ?? "missing");
  });
});

test.describe("sidebar legend on a phone", () => {
  test.use({ viewport: { width: 375, height: 812 }, hasTouch: true, isMobile: true });

  test.beforeEach(async ({ page }) => {
    await installSidebarMocks(page, { sessions: waitingFirst() });
    await page.goto("/");
    await openMobileSidebar(page);
  });

  test("? opens the legend as a sheet that the backdrop closes", async ({ page }) => {
    const toggle = page.getByRole("button", { name: "What do these symbols mean?" });
    await toggle.tap();
    const legend = page.getByTestId("sidebar-legend");
    await expect(legend).toBeVisible();
    await expect(legend).toHaveAttribute("role", "dialog");
    const box = await legend.boundingBox();
    expect(box && Math.round(box.y + box.height)).toBe(812);

    await page.touchscreen.tap(187, 40);
    await expect(legend).toBeHidden();
    await expect(toggle).toBeFocused();
  });

  test("a tap on a glyph opens the Session and shows no tooltip", async ({ page }) => {
    const glyph = page.getByTestId("sidebar-status-glyph").nth(1);
    await glyph.tap();
    await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
    await expect(page.getByRole("tooltip")).toHaveCount(0);
  });

  test("a long-press on a glyph still opens the row menu, with no tooltip while held", async ({ page }) => {
    const box = (await page.getByTestId("sidebar-status-glyph").nth(1).boundingBox())!;
    const [x, y] = [box.x + box.width / 2, box.y + box.height / 2];
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y, id: 1 }] });
    await expect(page.getByTestId("sidebar-context-menu")).toBeVisible();
    await expect(page.getByRole("tooltip")).toHaveCount(0);
    await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  });
});
