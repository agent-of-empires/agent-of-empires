// The shared sidebar view (#5896): one grouping and one sort every Client shows, the Daemon's to keep. The view
// payloads, poll adoption and the order arithmetic are unit-tested in src/hooks/__tests__/useSidebarModel.test.tsx;
// these cover the browser-only parts: dragging, polling and the toolbar layout.

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
    await expect(page.locator(TOGGLE)).toHaveText(/Repository.*Name/);
    // Headers and rows A to Z.
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["mike", "alpha", "zulu"]);
    // Rows stay draggable and nothing asks to switch first.
    await expect(page.locator(WRAPPERS)).toHaveCount(3);
    await expect(page.getByText(/Switch to Custom/)).toHaveCount(0);

    const wrappers = page.locator(WRAPPERS);
    await dragRow(page, wrappers.nth(2), wrappers.nth(1));

    await expect.poll(() => handle.viewPatches).toEqual([{ sort: "custom" }]);
    expect(handle.puts).toEqual([
      {
        order: [sessions[2]!, sessions[0]!, sessions[1]!].map(workspaceId),
        repositories: ["/tmp/repo-a", "/tmp/repo-b"],
      },
    ]);
    await expect(page.locator(TOGGLE)).toHaveText(/Repository.*Custom/);
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["mike", "zulu", "alpha"]);
    // Reloading reads the same order back from the Daemon.
    await page.reload();
    await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort", "custom");
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["mike", "zulu", "alpha"]);
  });

  test("another Client's pick shows up on the next poll, without a reload", async ({ page }) => {
    const handle = await open(page);
    await expect(page.locator(TOGGLE)).toHaveText(/Repository.*Custom/);
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["zulu", "alpha", "mike"]);
    handle.view = { group_by: "group", sort: "name" };
    await expect(page.locator(TOGGLE)).toHaveText(/My groups.*Name/, { timeout: 8_000 });
    await expect.poll(() => readVisibleSessionTitles(page)).toEqual(["alpha", "mike", "zulu"]);
    expect(handle.viewPatches).toEqual([]);
  });

  test("Reset to newest first clears both shared lists", async ({ page }) => {
    const handle = await open(page);
    await page.locator(TOGGLE).click();
    await page.getByRole("menuitem", { name: "Reset to newest first" }).click();
    await expect.poll(() => handle.puts).toEqual([{ order: [], repositories: [] }]);
  });

  // #1836: the list keeps its single separator under the toolbar.
  test("the view button replaces the old heading and keeps the toolbar on one row", async ({ page }) => {
    await open(page);
    const toggle = page.locator(TOGGLE);
    await expect(toggle).toBeVisible({ timeout: 8_000 });
    await expect(page.getByTestId("sidebar-axis-heading")).toHaveCount(0);
    const row = (await toggle.boundingBox())!;
    for (const name of ["Filter sessions", "New project session"]) {
      const box = (await page.getByRole("button", { name, exact: true }).boundingBox())!;
      expect(Math.abs(box.y + box.height / 2 - (row.y + row.height / 2))).toBeLessThan(2);
    }
    await expect(page.locator("div.overflow-y-auto.border-t")).toHaveCount(1);
  });
});
