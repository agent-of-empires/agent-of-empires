// User story (#1454): selecting a non-structured view session in the sidebar lands
// keyboard focus on its xterm textarea.
//
// First-open is already covered by useTerminal's autoFocus (it calls
// term.focus() when the socket opens). The gap this fix closes is the
// already-connected case: re-selecting the active session (or switching
// back to a persistent terminal) does not re-fire the socket open, so only
// the explicit focus dispatch from the select handler can refocus it. The
// blur-then-reselect below isolates exactly that dispatch.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

function activeElementInXterm(page: import("@playwright/test").Page) {
  return page.evaluate(() => {
    const active = document.activeElement as HTMLElement | null;
    return Boolean(active && active.closest(".xterm"));
  });
}

base(
  "desktop: re-selecting the active terminal session refocuses the textarea",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-focus-term" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-focus-term");
      if (!seeded) throw new Error("seeded session 'story-focus-term' missing");

      await page.goto(serve.baseUrl);
      const row = page
        .locator('[data-testid="sidebar-session-row"]')
        .first();
      await expect(row).toBeVisible({ timeout: 10_000 });

      // First select: navigate + connect; useTerminal autofocuses the xterm
      // textarea once the socket opens.
      await row.click();
      await expect(page).toHaveURL(
        new URL(
          `/session/${encodeURIComponent(seeded.id)}`,
          serve.baseUrl,
        ).toString(),
        { timeout: 10_000 },
      );
      await expect.poll(() => activeElementInXterm(page), { timeout: 10_000 }).toBe(
        true,
      );

      // Blur, then re-select the already-connected session. The socket does
      // not reopen, so a refocus here can only come from the select dispatch.
      await page.evaluate(() =>
        (document.activeElement as HTMLElement | null)?.blur(),
      );
      await expect.poll(() => activeElementInXterm(page)).toBe(false);

      await row.click();
      await expect.poll(() => activeElementInXterm(page), { timeout: 10_000 }).toBe(
        true,
      );
    } finally {
      await serve.stop();
    }
  },
);
