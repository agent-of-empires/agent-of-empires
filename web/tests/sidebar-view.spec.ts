// The shared sidebar view (#5896): one grouping and one sort every Client shows, the Daemon's to keep. The view
// payloads, poll adoption and the order arithmetic are unit-tested in src/hooks/__tests__/useSidebarModel.test.tsx;
// these cover the browser-only parts: dragging and polling.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { installSidebarMocks, workspaceId, type MockSessionInput } from "./helpers/sidebarMocks";
import { dragRow, readVisibleSessionTitles } from "./helpers/sidebar";

const TOGGLE = "[data-testid='sidebar-view-toggle']";
const WRAPPERS = "[aria-roledescription='Press and hold to reorder']";

const sessions: MockSessionInput[] = [
  { id: "s-z", title: "zulu", project_path: "/tmp/repo-b", branch: "feat/z" },
  { id: "s-a", title: "alpha", project_path: "/tmp/repo-b", branch: "feat/a" },
  { id: "s-m", title: "mike", project_path: "/tmp/repo-a", branch: "feat/m" },
];

/** The View options trigger names the shared grouping and sort it shows. */
async function expectView(page: Page, groupBy: string, sort: string, timeout?: number) {
  await expect(page.locator(TOGGLE)).toHaveAttribute("data-group-by", groupBy, { timeout });
  await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort", sort, { timeout });
}

async function open(page: Page, opts: Partial<Parameters<typeof installSidebarMocks>[1]> = {}) {
  const handle = await installSidebarMocks(page, {
    sessions,
    repositoryOrder: ["/tmp/repo-b", "/tmp/repo-a"],
    persistPutOrdering: true,
    ...opts,
  });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  return handle;
}

test.describe("shared sidebar view (#5896)", () => {
  test("a drag under Name switches to Custom from the order on screen, so only the dragged row moves", async ({
    page,
  }) => {
    const handle = await open(page, { view: { group_by: "project", sort: "name" } });
    await expectView(page, "project", "name");
    // Headers and rows A to Z.
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["mike", "alpha", "zulu"]);
    // Rows stay draggable and nothing asks to switch first.
    await expect(page.locator(WRAPPERS)).toHaveCount(3);
    await expect(page.getByText(/Switch to Custom/)).toHaveCount(0);

    const wrappers = page.locator(WRAPPERS);
    await dragRow(page, wrappers.nth(2), wrappers.nth(1));

    // Conditioned on the sort the drag began under, so a peer's later pick wins.
    await expect.poll(() => handle.viewPatches).toEqual([{ sort: "custom", base_sort: "name" }]);
    expect(handle.puts).toEqual([
      {
        order: [sessions[2]!, sessions[0]!, sessions[1]!].map(workspaceId),
        repositories: ["/tmp/repo-a", "/tmp/repo-b"],
      },
    ]);
    await expectView(page, "project", "custom");
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["mike", "zulu", "alpha"]);
    // Reloading reads the same order back from the Daemon.
    await page.reload();
    await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort", "custom");
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["mike", "zulu", "alpha"]);
  });

  test("another Client's pick shows up on the next poll, without a reload", async ({ page }) => {
    const handle = await open(page);
    await expectView(page, "project", "custom");
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["zulu", "alpha", "mike"]);
    handle.view = { group_by: "group", sort: "name" };
    await expectView(page, "group", "name", 8_000);
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["alpha", "mike", "zulu"]);
    expect(handle.viewPatches).toEqual([]);
  });

  test("another Client's change says why the list moved, and this dashboard's own change says nothing", async ({
    page,
  }) => {
    const handle = await open(page);
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["zulu", "alpha", "mike"]);
    handle.view = { group_by: "project", sort: "name", changed_by: "tui" };
    await expect(page.getByText("Sort changed in the TUI")).toBeVisible({ timeout: 8_000 });
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["mike", "alpha", "zulu"]);

    await page.locator(TOGGLE).click();
    await page.getByTestId("sidebar-view-group-group").click();
    await expect.poll(() => handle.viewPatches).toEqual([{ group_by: "group" }]);
    expect(handle.viewClients).toEqual(["web"]);
    await expectView(page, "group", "name");
    await expect(page.getByText(/Grouping changed/)).toHaveCount(0);
  });

  test("New group ends the Group by list, past a divider, where the arrow keys reach it", async ({ page }) => {
    await open(page);
    const toggle = page.locator(TOGGLE);
    await toggle.focus();
    await page.keyboard.press("Enter");
    const menu = page.getByRole("menu", { name: "View options" });
    await expect(menu.getByRole("menuitemradio", { name: "Repository" })).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("ArrowDown");
    await expect(menu.getByRole("menuitem", { name: "+ New group…" })).toBeFocused();
    await page.keyboard.press("Escape");
    await expect(menu).toHaveCount(0);
    await expect(toggle).toBeFocused();
  });

  for (const [name, opts, reason, canCreate] of [
    ["read-only", { readOnly: true }, "This Daemon is read-only.", false],
    ["CityHall", { cityhall: true }, "Managed by CityHall.", true],
  ] as const) {
    test(`a ${name} viewer lands on the current choice, hears why it is locked, and cannot pick`, async ({ page }) => {
      const handle = await open(page, opts);
      const toggle = page.locator(TOGGLE);
      await toggle.focus();
      await page.keyboard.press("Enter");
      const menu = page.getByRole("menu", { name: "View options" });
      await expect(menu).toHaveAccessibleDescription(reason);
      const current = menu.getByRole("menuitemradio", { name: "Repository" });
      await expect(current).toBeFocused();
      await expect(current).toBeChecked();
      for (const item of await menu.getByRole("menuitemradio").all()) {
        await expect(item).toBeDisabled();
        await expect(item).toHaveAccessibleDescription(reason);
      }
      // The arrow keys walk the locked choices rather than skipping them.
      await page.keyboard.press("ArrowDown");
      const groups = menu.getByRole("menuitemradio", { name: "Groups" });
      await expect(groups).toBeFocused();
      await page.keyboard.press("Enter");
      await groups.click({ force: true });
      await expect(menu).toBeVisible();
      await expect(current).toBeChecked();
      const newGroup = menu.getByRole("menuitem", { name: "+ New group…" });
      if (canCreate) await expect(newGroup).toBeEnabled();
      else await expect(newGroup).toBeDisabled();
      expect(handle.viewPatches).toEqual([]);
    });
  }

  test("New group creates the Group with the selected Session and at least one Session", async ({ page }) => {
    const handle = await open(page);
    await page.locator("[data-testid='sidebar-session-row']", { hasText: "alpha" }).click();
    await expect(page).toHaveURL(/\/session\/s-a$/);
    await page.locator(TOGGLE).click();
    await page.getByRole("menuitem", { name: "+ New group…" }).click();
    const dialog = page.getByRole("dialog", { name: "New group" });
    await expect(dialog.getByRole("checkbox", { name: "alpha" })).toBeChecked();
    await expect(dialog.getByRole("checkbox", { name: "zulu" })).not.toBeChecked();
    await dialog.getByRole("checkbox", { name: "alpha" }).uncheck();
    await dialog.getByLabel("Group name").fill("ops");
    await dialog.getByRole("button", { name: "Create" }).click();
    await expect(dialog.getByRole("alert")).toHaveText("Pick at least one session.");
    expect(handle.groupCreates).toEqual([]);
    await dialog.getByRole("checkbox", { name: "mike" }).check();
    await dialog.getByLabel("Group name").press("Enter");
    await expect(dialog).toHaveCount(0);
    expect(handle.groupCreates).toEqual([{ profile: "default", path: "ops", sessions: ["s-m"] }]);
    await page.locator(TOGGLE).click();
    await page.getByTestId("sidebar-view-group-group").click();
    await expect(page.getByTestId("sidebar-group-header").filter({ hasText: "ops" })).toBeVisible({ timeout: 8_000 });
  });

  test("the sidebar toggle counts who needs you only while the sidebar is hidden", async ({ page }) => {
    await open(page, {
      sessions: [
        { ...sessions[0]!, fields: { status: "Waiting" } },
        { ...sessions[1]!, fields: { status: "Error" } },
        sessions[2]!,
      ],
    });
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toHaveLength(3);
    const badge = page.getByTestId("topbar-needs-you-badge");
    await expect(badge).toHaveCount(0);
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await expect(badge).toHaveText("2");
    await expect(page.getByRole("button", { name: "Toggle sidebar, 2 need you" })).toBeVisible();
  });
});
