// User story: a sidebar drag-reorder survives a full page reload
// (#1419). The drag specs in this family round-trip one drag and
// confirm the PUT body, but they do not refresh the page; a regression
// in the bootstrap path that re-derives ordering from `created_at`
// instead of honoring the server-supplied `workspace_ordering` would
// ship green there.
//
// This spec drags the bottom row to the top, awaits the PUT round-
// trip, reloads the page, and asserts the new order paints from the
// initial `GET /api/sessions` response (not after a delayed
// client-side sort). The mock is stateful: a successful PUT replaces
// the ordering served by subsequent GETs (`persistPutOrdering`), so
// the reload assertion exercises the same contract as the real server.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { installSidebarMocks, threeSessionsInOneRepo, workspaceId } from "./helpers/sidebarMocks";

async function readVisibleSessionTitles(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const rows = Array.from(document.querySelectorAll<HTMLElement>("[data-testid='sidebar-session-row']"));
    return rows.map((r) => r.querySelector("span.truncate[title]")?.getAttribute("title") ?? "").filter(Boolean);
  });
}

test("sidebar reorder persists across a full page reload", async ({ page }) => {
  const sessions = threeSessionsInOneRepo();
  const handle = await installSidebarMocks(page, {
    sessions,
    persistPutOrdering: true,
  });

  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["alpha", "beta", "gamma"]);

  const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
  await expect(wrappers).toHaveCount(3);

  // Wait for the PUT so we know the (mock) server persisted before the
  // reload.
  const putWait = page.waitForResponse(
    (r) => r.url().endsWith("/api/workspace-ordering") && r.request().method() === "PUT" && r.status() < 400,
    { timeout: 8_000 },
  );

  // Press-and-hold on gamma's row, then drag up onto alpha's row. The
  // press targets the sortable wrapper near its right edge so dnd-kit's
  // MouseSensor (not the inner Link) receives the gesture; see
  // sidebar-drag-reorder.spec.ts for the full rationale.
  const sourceBox = await wrappers.nth(2).boundingBox();
  const targetBox = await wrappers.nth(0).boundingBox();
  if (!sourceBox || !targetBox) throw new Error("row box missing");

  await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
  await page.mouse.down();
  await page.waitForTimeout(250);
  await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });
  await page.mouse.up();

  await putWait;
  expect(handle.puts.at(-1)?.order).toEqual([
    workspaceId(sessions[2]!),
    workspaceId(sessions[0]!),
    workspaceId(sessions[1]!),
  ]);

  await expect.poll(() => readVisibleSessionTitles(page), { timeout: 4_000 }).toEqual(["gamma", "alpha", "beta"]);

  // Reload the whole page. The new visual order must come from the
  // initial server response, not from a delayed PUT or sort. Poll
  // because the very first paint can briefly show the bootstrap shell
  // before sessions land.
  await page.reload();
  await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(["gamma", "alpha", "beta"]);
});
