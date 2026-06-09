// User story: a sidebar drag-reorder survives a full page reload
// (#1419). The live `workspace-ordering.spec.ts` round-trips one drag
// and confirms `GET /api/sessions` returns the merged ordering, but
// it does not refresh the page; a regression in the bootstrap path
// that re-derives ordering from `created_at` instead of honoring the
// server-supplied `workspace_ordering` would ship green there.
//
// This spec drags the bottom row to the top, awaits the PUT round-
// trip, reloads the page, and asserts the new order paints from the
// initial `GET /api/sessions` response (not after a delayed
// client-side sort).

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions } from "../../helpers/aoeServe";
import { readVisibleSessionTitles, seedSessionsInRepo } from "../../helpers/sidebar";

base("sidebar reorder persists across a full page reload", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionsInRepo({ titles: ["alpha", "beta", "gamma"] }),
  });

  try {
    const seeded = await listSessions(serve.baseUrl);
    expect(seeded).toHaveLength(3);

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto(`${serve.baseUrl}/`);

    await expect
      .poll(() => readVisibleSessionTitles(page), { timeout: 8_000 })
      .toEqual(expect.arrayContaining(["alpha", "beta", "gamma"]));
    const initial = await readVisibleSessionTitles(page);
    expect(initial).toHaveLength(3);

    const wrappers = page.locator("[aria-roledescription='Press and hold to reorder']");
    await expect(wrappers).toHaveCount(3);

    // Wait for the PUT so we know the server persisted before the
    // reload. The route-attach captures it without altering response.
    const putWait = page.waitForResponse(
      (r) => r.url().endsWith("/api/workspace-ordering") && r.request().method() === "PUT" && r.status() < 400,
      { timeout: 8_000 },
    );

    const sourceBox = await wrappers.nth(2).boundingBox();
    const targetBox = await wrappers.nth(0).boundingBox();
    if (!sourceBox || !targetBox) throw new Error("row box missing");

    await page.mouse.move(sourceBox.x + sourceBox.width - 4, sourceBox.y + sourceBox.height / 2);
    await page.mouse.down();
    await page.waitForTimeout(250);
    await page.mouse.move(targetBox.x + targetBox.width / 2, targetBox.y + targetBox.height / 2, { steps: 12 });
    await page.mouse.up();

    await putWait;

    const expectedAfter = [initial[2], initial[0], initial[1]];
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 4_000 }).toEqual(expectedAfter);

    // Reload the whole page. The new visual order must come from the
    // initial server response, not from a delayed PUT or sort. Poll
    // because the very first paint can briefly show the bootstrap
    // shell before sessions land.
    await page.reload();
    await expect.poll(() => readVisibleSessionTitles(page), { timeout: 8_000 }).toEqual(expectedAfter);
  } finally {
    await serve.stop();
  }
});
