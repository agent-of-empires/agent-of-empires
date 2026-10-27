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

    // Shift+F10 opens the menu with focus on its first item; Escape closes it and focus returns to the header button.
    await expect(toggle(0)).toHaveAttribute("aria-keyshortcuts", "Shift+F10");
    await page.keyboard.press("Shift+F10");
    const menu = page.getByTestId("sidebar-group-context-menu");
    await expect(menu).toBeVisible();
    await expect(menu.getByRole("button").first()).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(toggle(0)).toBeFocused();
  });
}

// Nested lists put a Group's rows inside its parent's. Repo "/tmp/a" with Group "b" and repo
// "/tmp/a::b" would share a rows id if "::" weren't only ever the separator.
const nested: MockSessionInput[] = [
  { id: "s-n1", title: "n-one", project_path: "/tmp/a", branch: "n/one", group: "b", remote_owner: "acme" },
  { id: "s-n2", title: "n-two", project_path: "/tmp/a::b", branch: "n/two", group: "g", remote_owner: "acme" },
];

for (const axis of ["repo+group", "org"] as const) {
  test(`arrow keys reach a nested header and its rows (${axis} axis)`, async ({ page }) => {
    await installSidebarMocks(page, { sessions: nested });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await showAxis(page, axis);

    const ids = await page
      .locator("[data-testid='sidebar-group-header'] button[aria-controls]")
      .evaluateAll((buttons) => buttons.map((b) => b.getAttribute("aria-controls")));
    expect(ids.length).toBeGreaterThan(2);
    expect(new Set(ids).size, ids.join(" ")).toBe(ids.length);

    const press = async (key: string, focused: Locator) => {
      await page.keyboard.press(key);
      await expect(focused, key).toBeFocused();
    };
    const parent = page.locator("[data-testid='sidebar-group-header'] button[aria-expanded]").first();
    const child = page.locator(`[id="${await parent.getAttribute("aria-controls")}"] button[aria-expanded]`).first();
    const childRows = page.locator(
      `[id="${await child.getAttribute("aria-controls")}"] [data-testid="sidebar-session-row"]`,
    );

    // Parent header, Right to its child header, Right to the first row, Left back to the child.
    await parent.focus();
    await press("ArrowRight", child);
    await press("ArrowRight", childRows.first());
    await press("ArrowLeft", child);
    // Left collapses the child, then Left again on the collapsed child goes up to its parent.
    await press("ArrowLeft", child);
    await expect(child).toHaveAttribute("aria-expanded", "false");
    await press("ArrowLeft", parent);
  });
}

test("the Snoozed & archived toggle is a header for the arrow keys", async ({ page }) => {
  const sessions: MockSessionInput[] = [
    { id: "s-live", title: "live", project_path: "/tmp/repo-a", branch: "a/live" },
    {
      id: "s-old",
      title: "old",
      project_path: "/tmp/repo-b",
      branch: "b/old",
      fields: { archived_at: "2026-01-01T00:00:00Z" },
    },
  ];
  await installSidebarMocks(page, { sessions });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const press = async (key: string, focused: Locator) => {
    await page.keyboard.press(key);
    await expect(focused, key).toBeFocused();
  };
  const toggle = page.getByTestId("sidebar-sunk-toggle");
  const live = page.getByTestId("sidebar-session-row").filter({ hasText: "live" });
  const old = page.getByTestId("sidebar-session-row").filter({ hasText: "old" });

  // Down from the last live row stops on the toggle; Right opens it, Right again reaches its row.
  await live.focus();
  await press("ArrowDown", toggle);
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await press("ArrowRight", toggle);
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await press("ArrowRight", old);
  // Left on an archived row comes back to the toggle, which Left then folds.
  await press("ArrowLeft", toggle);
  await press("ArrowLeft", toggle);
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
});

test("Archive all that empties a Group moves focus to the next header", async ({ page }) => {
  // `fields` is merged into each list response, so an archive shows up on the next refresh.
  const sessions: MockSessionInput[] = [
    { id: "s-a", title: "a-only", project_path: "/tmp/repo-a", branch: "a/only", fields: {} },
    { id: "s-b", title: "b-only", project_path: "/tmp/repo-b", branch: "b/only", fields: {} },
  ];
  await installSidebarMocks(page, { sessions });
  await page.route("**/api/sessions/*/archive", async (r) => {
    const id = new URL(r.request().url()).pathname.split("/")[3];
    const archived_at = new Date().toISOString();
    Object.assign(sessions.find((s) => s.id === id)?.fields ?? {}, { archived_at });
    await r.fulfill({ json: { id, archived_at } });
  });
  page.on("dialog", (dialog) => void dialog.accept());
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const header = (path: string) =>
    page.locator(`[data-testid="sidebar-group-header"][data-group-id="${path}"] button[aria-expanded]`);
  await header("/tmp/repo-a").focus();
  await page.keyboard.press("Shift+F10");
  const archiveAll = page.getByTestId("sidebar-group-context-menu-archive-all");
  await expect(archiveAll).toBeVisible();
  // The menu focuses its first item a frame after opening; wait for that, or it takes focus back.
  await expect(page.getByTestId("sidebar-group-context-menu").getByRole("button").first()).toBeFocused();
  await archiveAll.focus();
  await Promise.all([page.waitForRequest("**/api/sessions/s-a/archive"), page.keyboard.press("Enter")]);

  await expect(header("/tmp/repo-a")).toHaveCount(0);
  await expect(header("/tmp/repo-b")).toBeFocused();
});
