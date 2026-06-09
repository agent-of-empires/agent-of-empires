// Live coverage for dragging project/group headers to reorder repo
// groups in the sidebar (#1644). Group order is persisted client-only in
// localStorage (see `repoGroupOrder.ts` + `useRepoGroups.ts`), so unlike
// `workspace-ordering.spec.ts` there is no server PUT to assert; the
// round-trip we care about is "drag, then reload, order survives".
//
// The grip is a dedicated drag handle on each real group header
// (`data-testid='sidebar-group-drag-handle'`); the rest of the header
// keeps its expand/collapse + context-menu behavior. dnd-kit's
// MouseSensor activates on an 8px distance, so the drag is
// mouse.down on the grip → mouse.move past 8px in steps over the target
// header → mouse.up. Synthetic groups have no grip and stay pinned, and
// group drag is disabled in last-activity sort mode (the order is
// computed there).

import { test as base, expect } from "@playwright/test";
import type { Page } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";
import { seedRepos } from "../helpers/sidebar";

const TOGGLE = "[data-testid='sidebar-sort-toggle']";

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

base.describe("sidebar group-header reorder (#1644)", () => {
  base("drag a group header to reorder, order persists across reload", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedRepos([
        { titles: ["alpha-session"], subdir: "repo-alpha" },
        { titles: ["beta-session"], subdir: "repo-beta" },
      ]),
    });

    try {
      await page.setViewportSize({ width: 1280, height: 720 });
      await page.goto(`${serve.baseUrl}/`);

      const grips = page.locator("[data-testid='sidebar-group-drag-handle']");
      // Cold-start under load can lag past Playwright's default; the
      // other live sidebar specs bump first-paint waits for the same
      // reason. 15s covers a heavily-loaded CI worker.
      await expect(grips).toHaveCount(2, { timeout: 15_000 });

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
      // Cold-start under load can lag past Playwright's default; the
      // other live sidebar specs bump first-paint waits for the same
      // reason. 15s covers a heavily-loaded CI worker.
      await expect(grips).toHaveCount(2, { timeout: 15_000 });
      await expect.poll(() => readGroupNames(page), { timeout: 4_000 }).toEqual(expected);
    } finally {
      await serve.stop();
    }
  });

  base("group drag handles are absent in last-activity sort mode", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedRepos([
        { titles: ["alpha-session"], subdir: "repo-alpha" },
        { titles: ["beta-session"], subdir: "repo-beta" },
      ]),
    });

    try {
      await page.setViewportSize({ width: 1280, height: 720 });
      await page.goto(`${serve.baseUrl}/`);

      const grips = page.locator("[data-testid='sidebar-group-drag-handle']");
      // Cold-start under load can lag past Playwright's default; the
      // other live sidebar specs bump first-paint waits for the same
      // reason. 15s covers a heavily-loaded CI worker.
      await expect(grips).toHaveCount(2, { timeout: 15_000 });

      // Flip to last-activity sort; the order is computed there, so the
      // grips disappear, matching how within-group row drag is gated.
      await selectSortMode(page, "lastActivity");
      await expect(grips).toHaveCount(0);
    } finally {
      await serve.stop();
    }
  });
});
