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
});
