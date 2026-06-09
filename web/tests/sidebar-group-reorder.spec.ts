// Mocked coverage for dragging project/group headers to reorder repo
// groups in the sidebar (#1644), ported from the live spec. Group order
// is persisted client-only in localStorage (see `repoGroupOrder.ts` +
// `useRepoGroups.ts`), so unlike the workspace-row reorder there is no
// server PUT to assert; the round-trip we care about is "drag, then
// reload, order survives", which works against a fully stubbed /api.
//
// The grip is a dedicated drag handle on each real group header
// (`data-testid='sidebar-group-drag-handle'`); the rest of the header
// keeps its expand/collapse + context-menu behavior. dnd-kit's
// MouseSensor activates on an 8px distance, so the drag is
// mouse.down on the grip -> mouse.move past 8px in steps over the target
// header -> mouse.up. Synthetic groups have no grip and stay pinned, and
// group drag is disabled in last-activity sort mode (the order is
// computed there).

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

    const grips = page.locator("[data-testid='sidebar-group-drag-handle']");
    await expect(grips).toHaveCount(2);

    const before = await readGroupNames(page);
    expect(before).toHaveLength(2);

    // Drag the bottom group's grip up onto the top group's header.
    const sourceGrip = await grips.nth(1).boundingBox();
    const targetHeader = await page.locator("[data-testid='sidebar-group-header']").nth(0).boundingBox();
    if (!sourceGrip || !targetHeader) throw new Error("drag boxes missing");

    await page.mouse.move(sourceGrip.x + sourceGrip.width / 2, sourceGrip.y + sourceGrip.height / 2);
    await page.mouse.down();
    await page.mouse.move(targetHeader.x + targetHeader.width / 2, targetHeader.y + targetHeader.height / 3, {
      steps: 12,
    });
    await page.mouse.up();

    const expected = [before[1], before[0]];
    await expect.poll(() => readGroupNames(page), { timeout: 4_000 }).toEqual(expected);

    // The order is client-only; a reload re-reads it from localStorage.
    await page.reload();
    await expect(grips).toHaveCount(2);
    await expect.poll(() => readGroupNames(page), { timeout: 4_000 }).toEqual(expected);
  });

  test("group drag handles are absent in last-activity sort mode", async ({ page }) => {
    await installSidebarMocks(page, { sessions: twoRepoSessions() });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");

    const grips = page.locator("[data-testid='sidebar-group-drag-handle']");
    await expect(grips).toHaveCount(2);

    // Flip to last-activity sort; the order is computed there, so the
    // grips disappear, matching how within-group row drag is gated.
    await selectSortMode(page, "lastActivity");
    await expect(grips).toHaveCount(0);
  });
});
