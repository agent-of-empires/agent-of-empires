// Mocked coverage for dragging project/group headers to reorder repo
// groups in the sidebar (#1644), ported from the live spec. Group order
// is persisted client-only in localStorage (see `repoGroupOrder.ts` +
// `useRepoGroups.ts`), so unlike the workspace-row reorder there is no
// server PUT to assert; the round-trip we care about is "drag, then
// reload, order survives", which works against a fully stubbed /api.
//
// There is no dedicated grip anymore (#2207): the whole real group header
// is the drag activator, while the header keeps its expand/collapse +
// context-menu behavior. A draggable header carries `data-draggable='true'`.
// dnd-kit's MouseSensor activates on an 8px distance, so the drag is
// mouse.down on the header -> mouse.move past 8px in steps over the target
// header -> mouse.up. Synthetic groups are not draggable and stay pinned,
// and group drag is disabled in last-activity sort mode (the order is
// computed there), where the header drops `data-draggable`.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { installSidebarMocks, type MockSessionInput } from "./helpers/sidebarMocks";

const TOGGLE = "[data-testid='sidebar-sort-toggle']";

function twoRepoSessions(): MockSessionInput[] {
  return [
    { id: "s-a", title: "alpha-session", project_path: "/tmp/repo-alpha", branch: "feat/a" },
    { id: "s-b", title: "beta-session", project_path: "/tmp/repo-beta", branch: "feat/b" },
  ];
}

async function readGroupNames(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const headers = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-group-header']"));
    return headers.map((h) => h.querySelector("span[title]")?.textContent?.trim() ?? "").filter(Boolean);
  });
}

async function selectSortMode(page: Page, mode: "manual" | "lastActivity") {
  await page.locator(TOGGLE).click();
  await page.locator(`[data-testid='sidebar-sort-option-${mode}']`).click();
  await expect(page.locator(TOGGLE)).toHaveAttribute("data-sort-mode", mode);
}

test.describe("sidebar group-header reorder (#1644)", () => {
  test("drag a group header to reorder, order persists across reload", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const draggable = page.locator("[data-testid='sidebar-group-header'][data-draggable='true']");
    await expect(draggable).toHaveCount(2);

    const before = await readGroupNames(page);
    expect(before).toHaveLength(2);

    // Drag the bottom group's header up onto the top group's header. Grab
    // near the left (icon/name) to stay clear of the new-session button.
    const source = await draggable.nth(1).boundingBox();
    const targetHeader = await page.locator("[data-testid='sidebar-group-header']").nth(0).boundingBox();
    if (!source || !targetHeader) throw new Error("drag boxes missing");

    await page.mouse.move(source.x + 40, source.y + source.height / 2);
    await page.mouse.down();
    await page.mouse.move(targetHeader.x + 40, targetHeader.y + targetHeader.height / 3, {
      steps: 12,
    });
    await page.mouse.up();

    const expected = [before[1], before[0]];
    await expect.poll(() => readGroupNames(page), { timeout: 4_000 }).toEqual(expected);

    // The order is client-only; a reload re-reads it from localStorage.
    await page.reload();
    await expect(draggable).toHaveCount(2);
    await expect.poll(() => readGroupNames(page), { timeout: 4_000 }).toEqual(expected);
  });

  test("group headers are not draggable in last-activity sort mode", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const draggable = page.locator("[data-testid='sidebar-group-header'][data-draggable='true']");
    await expect(draggable).toHaveCount(2);

    // Flip to last-activity sort; the order is computed there, so the
    // headers lose their drag wiring, matching how within-group row drag
    // is gated.
    await selectSortMode(page, "lastActivity");
    await expect(draggable).toHaveCount(0);
  });
});
