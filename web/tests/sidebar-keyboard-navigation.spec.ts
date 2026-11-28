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

test("arrow keys up a scrolled list keep the focused row clear of the pinned title row", async ({ page }) => {
  const sessions = Array.from({ length: 20 }, (_, i) => ({
    id: `s-${i}`,
    title: `row-${i}`,
    project_path: "/tmp/repo",
    branch: `b/${i}`,
  }));
  await installSidebarMocks(page, { sessions });
  await page.setViewportSize({ width: 1280, height: 500 });
  await page.goto("/");

  const rows = page.getByTestId("sidebar-session-row");
  await expect(rows).toHaveCount(20);
  await rows.last().focus();
  const pinned = page.locator('[aria-label="Sessions sidebar"] div.sticky').first();
  for (let i = 0; i < 19; i++) {
    await page.keyboard.press("ArrowUp");
    const box = (await pinned.boundingBox())!;
    const focusedTop = await page.evaluate(() => document.activeElement!.getBoundingClientRect().top);
    expect(focusedTop).toBeGreaterThanOrEqual(box.y + box.height - 1);
  }
});

// Two repos of two rows each: under Groups also two Groups, so either grouping shows two headers. Under
// Repository they stay out of Groups, which would nest a header of their own inside each repository.
const twoByTwo = (grouped: boolean): MockSessionInput[] =>
  [
    { id: "s-a1", title: "a-one", project_path: "/tmp/repo-a", branch: "a/one", group: "g-a" },
    { id: "s-a2", title: "a-two", project_path: "/tmp/repo-a", branch: "a/two", group: "g-a" },
    { id: "s-b1", title: "b-one", project_path: "/tmp/repo-b", branch: "b/one", group: "g-b" },
    { id: "s-b2", title: "b-two", project_path: "/tmp/repo-b", branch: "b/two", group: "g-b" },
  ].map((s) => (grouped ? s : { ...s, group: undefined }));

for (const [groupBy, compact] of [
  ["project", false],
  ["group", false],
  ["project", true],
] as const) {
  test(`headers are one tab stop each and share the arrow keys with rows (${groupBy}${compact ? ", compact" : ""})`, async ({
    page,
  }) => {
    await installSidebarMocks(page, {
      sessions: twoByTwo(groupBy === "group"),
      // Headers never moved sit newest first; listing them keeps repo-a on top.
      repositoryOrder: ["/tmp/repo-a", "/tmp/repo-b"],
      view: { group_by: groupBy, sort: "custom" },
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    if (compact) {
      await page.getByRole("separator", { name: "Resize sidebar" }).focus();
      await page.keyboard.press("Home");
      await expect(page.locator('[data-tour="sidebar"]')).toHaveAttribute("data-compact", "true");
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
  { id: "s-n1", title: "n-one", project_path: "/tmp/a", branch: "n/one", group: "b" },
  { id: "s-n2", title: "n-two", project_path: "/tmp/a::b", branch: "n/two", group: "g" },
];

{
  test("arrow keys reach a nested header and its rows (Repository)", async ({ page }) => {
    await installSidebarMocks(page, { sessions: nested });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    // Each repository and its Group.
    await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(4);

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
  // The live session's Group is named like the section, so a shared rows id would send Right into it.
  const sessions: MockSessionInput[] = [
    { id: "s-live", title: "live", project_path: "/tmp/repo-a", branch: "a/live", group: "snoozed-and-archived" },
    {
      id: "s-old",
      title: "old",
      project_path: "/tmp/repo-b",
      branch: "b/old",
      fields: { archived_at: "2026-01-01T00:00:00Z" },
    },
  ];
  await installSidebarMocks(page, { sessions, view: { group_by: "group", sort: "custom" } });
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

test.describe("Archive all that empties a Group", () => {
  // `fields` is merged into each list response, so an archive shows up on the next refresh.
  // `hold` keeps an archive reply back until released.
  async function setup(page: Page, sessions: MockSessionInput[], hold?: Promise<void>) {
    // Repository headers in the order given, rather than newest first.
    await installSidebarMocks(page, { sessions, repositoryOrder: sessions.map((s) => s.project_path) });
    await page.route("**/api/sessions/*/archive", async (r) => {
      const id = new URL(r.request().url()).pathname.split("/")[3];
      await hold;
      const archived_at = new Date().toISOString();
      Object.assign(sessions.find((s) => s.id === id)?.fields ?? {}, { archived_at });
      await r.fulfill({ json: { id, archived_at } });
    });
    page.on("dialog", (dialog) => void dialog.accept());
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
  }
  const header = (page: Page, path: string) =>
    page.locator(`[data-testid="sidebar-group-header"][data-group-id="${path}"] button[aria-expanded]`).first();
  async function archiveAll(page: Page, from: Locator, sessionId: string) {
    await from.focus();
    await page.keyboard.press("Shift+F10");
    const item = page.getByTestId("sidebar-group-context-menu-archive-all");
    await expect(item).toBeVisible();
    // The menu focuses its first item a frame after opening; wait for that, or it takes focus back.
    await expect(page.getByTestId("sidebar-group-context-menu").getByRole("button").first()).toBeFocused();
    await item.focus();
    await Promise.all([page.waitForRequest(`**/api/sessions/${sessionId}/archive`), page.keyboard.press("Enter")]);
  }
  const one = (id: string, repo: string, fields: Record<string, unknown> = {}): MockSessionInput => ({
    id,
    title: `${repo}-only`,
    project_path: `/tmp/${repo}`,
    branch: `${repo}/only`,
    fields,
  });

  test("moves focus to the next Group header, then from the last Group to the previous one", async ({ page }) => {
    await setup(page, [one("s-a", "repo-a"), one("s-b", "repo-b"), one("s-c", "repo-c")]);
    await archiveAll(page, header(page, "/tmp/repo-a"), "s-a");
    await expect(header(page, "/tmp/repo-a")).toHaveCount(0);
    await expect(header(page, "/tmp/repo-b")).toBeFocused();
    // Snoozed & archived now exists below the last Group; it isn't a Group, so focus goes up instead.
    await expect(page.getByTestId("sidebar-sunk-toggle")).toBeVisible();
    await archiveAll(page, header(page, "/tmp/repo-c"), "s-c");
    await expect(header(page, "/tmp/repo-c")).toHaveCount(0);
    await expect(header(page, "/tmp/repo-b")).toBeFocused();
  });

  test("goes past a parent header that leaves with its last subgroup", async ({ page }) => {
    await setup(page, [
      { ...one("s-w", "repo-w"), group: "g-w" },
      { ...one("s-x", "repo-x"), group: "g-x" },
    ]);
    const parentX = header(page, "/tmp/repo-x");
    const childX = page.locator(`[id="${await parentX.getAttribute("aria-controls")}"] button[aria-expanded]`).first();
    const parentW = header(page, "/tmp/repo-w");
    const childW = page.locator(`[id="${await parentW.getAttribute("aria-controls")}"] button[aria-expanded]`).first();
    await archiveAll(page, childX, "s-x");
    // repo-x's header goes with its only subgroup; focus lands on the nearest Group header above.
    await expect(parentX).toHaveCount(0);
    await expect(childW).toBeFocused();
  });

  test("leaves focus alone when it moved before the Group emptied", async ({ page }) => {
    let release: () => void = () => {};
    const hold = new Promise<void>((resolve) => (release = resolve));
    await setup(page, [one("s-a", "repo-a"), one("s-b", "repo-b")], hold);
    await archiveAll(page, header(page, "/tmp/repo-a"), "s-a");
    // A click on nothing focusable moves focus to the page while the archive is still out.
    await page.mouse.click(900, 650);
    release();
    await expect(header(page, "/tmp/repo-a")).toHaveCount(0);
    await expect(header(page, "/tmp/repo-b")).not.toBeFocused();
  });
});
