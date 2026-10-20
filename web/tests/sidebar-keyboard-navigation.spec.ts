import { test, expect } from "./helpers/mockedTest";
import type { Locator, Page } from "@playwright/test";
import { installSidebarMocks, threeSessionsInOneRepo, type MockSessionInput } from "./helpers/sidebarMocks";

test("arrow keys move focus through the session list and Enter opens the focused session", async ({ page }) => {
  await installSidebarMocks(page, {
    sessions: threeSessionsInOneRepo(),
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(3);
  await rows.nth(0).focus();
  // Headers are in the list too, so Up from the first row reaches its header.
  await page.keyboard.press("ArrowUp");
  await expect(page.locator("[data-testid='sidebar-group-header'] button[aria-expanded]")).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(0)).toBeFocused();

  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(1)).toBeFocused();
  await expect(page).toHaveURL("/");

  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(2)).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(2)).toBeFocused();

  await page.keyboard.press("ArrowUp");
  await expect(rows.nth(1)).toBeFocused();
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/\/session\/s-b$/, { timeout: 5_000 });
});

test("arrow keys skip deleting session rows", async ({ page }) => {
  const sessions = threeSessionsInOneRepo().map((session, index) =>
    index === 1 ? { ...session, fields: { status: "Deleting" } } : session,
  );
  await installSidebarMocks(page, { sessions });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(3);
  await expect(rows.nth(1)).toHaveAttribute("aria-disabled", "true");
  await rows.nth(0).focus();
  await page.keyboard.press("ArrowDown");
  await expect(rows.nth(2)).toBeFocused();
  await expect(page).toHaveURL("/");
});

// Two repos that are also two Groups, so both axes show two headers of two rows each.
const twoByTwo: MockSessionInput[] = [
  { id: "s-a1", title: "a-one", project_path: "/tmp/repo-a", branch: "a/one", group: "g-a" },
  { id: "s-a2", title: "a-two", project_path: "/tmp/repo-a", branch: "a/two", group: "g-a" },
  { id: "s-b1", title: "b-one", project_path: "/tmp/repo-b", branch: "b/one", group: "g-b" },
  { id: "s-b2", title: "b-two", project_path: "/tmp/repo-b", branch: "b/two", group: "g-b" },
];

async function showAxis(page: Page, axis: string) {
  const toggle = page.getByTestId("sidebar-axis-toggle");
  for (let i = 0; i < 4 && (await toggle.getAttribute("data-axis")) !== axis; i++) {
    const current = await toggle.getAttribute("data-axis");
    await toggle.click();
    await expect(toggle).not.toHaveAttribute("data-axis", current ?? "");
  }
  await expect(toggle).toHaveAttribute("data-axis", axis);
}

for (const [axis, compact] of [
  ["repo", false],
  ["group", false],
  ["repo", true],
] as const) {
  test(`headers are one tab stop each and share the arrow keys with rows (${axis} axis${compact ? ", compact" : ""})`, async ({
    page,
  }) => {
    await installSidebarMocks(page, { sessions: twoByTwo });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await showAxis(page, axis);
    if (compact) {
      await page.getByRole("button", { name: "Compact sidebar" }).click();
      await expect(page.getByRole("button", { name: "Expand sidebar" })).toBeVisible();
    }
    const headers = page.getByTestId("sidebar-group-header");
    await expect(headers).toHaveCount(2);
    const toggle = (i: number): Locator => headers.nth(i).locator("button[aria-expanded]");
    const press = async (key: string, focused: Locator) => {
      await page.keyboard.press(key);
      await expect(focused, key).toBeFocused();
    };

    // One stop per header: Shift+Tab from its first row goes "+" (absent in compact), then the header button, then out.
    const controlled = await toggle(0).getAttribute("aria-controls");
    const firstRow = page.locator(`[id="${controlled}"] [data-testid="sidebar-session-row"]`).first();
    await expect(firstRow).toBeVisible();
    await firstRow.focus();
    if (!compact) await press("Shift+Tab", headers.nth(0).getByRole("button", { name: /New session in/ }));
    await press("Shift+Tab", toggle(0));
    await page.keyboard.press("Shift+Tab");
    expect(await headers.nth(0).evaluate((h) => h.contains(document.activeElement))).toBe(false);

    // Enter and Space toggle.
    await toggle(0).focus();
    await page.keyboard.press("Enter");
    await expect(toggle(0)).toHaveAttribute("aria-expanded", "false");
    await page.keyboard.press(" ");
    await expect(toggle(0)).toHaveAttribute("aria-expanded", "true");

    // A modifier leaves the arrows to the browser and other shortcuts. (Not Left or Right: Alt+Left is Back.)
    for (const modifier of ["Alt", "Control", "Meta", "Shift"]) await press(`${modifier}+ArrowDown`, toggle(0));

    // Down walks header, row, row, next header; Left on a row returns to its header.
    const rowsOf = async (i: number) =>
      page.locator(`[id="${await toggle(i).getAttribute("aria-controls")}"] [data-testid="sidebar-session-row"]`);
    const first = await rowsOf(0);
    const second = await rowsOf(1);
    await press("ArrowDown", first.nth(0));
    await press("ArrowDown", first.nth(1));
    await press("ArrowDown", toggle(1));
    await press("ArrowDown", second.nth(0));
    await press("ArrowLeft", toggle(1));

    // Left collapses, Right expands, Right again enters the rows.
    await page.keyboard.press("ArrowLeft");
    await expect(toggle(1)).toHaveAttribute("aria-expanded", "false");
    await page.keyboard.press("ArrowRight");
    await expect(toggle(1)).toHaveAttribute("aria-expanded", "true");
    await press("ArrowRight", second.nth(0));

    // A collapsed Group's rows are skipped.
    await toggle(0).focus();
    await page.keyboard.press("ArrowLeft");
    await expect(toggle(0)).toHaveAttribute("aria-expanded", "false");
    await press("ArrowDown", toggle(1));
    await press("ArrowUp", toggle(0));

    // Shift+F10 opens the menu; Escape closes it and leaves focus on the header button.
    await expect(toggle(0)).toHaveAttribute("aria-keyshortcuts", "Shift+F10");
    await page.keyboard.press("Shift+F10");
    const menu = page.getByTestId("sidebar-group-context-menu");
    await expect(menu).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(toggle(0)).toBeFocused();
  });
}
