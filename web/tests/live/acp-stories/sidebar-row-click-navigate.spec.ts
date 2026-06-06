// User story: clicking a sidebar session row navigates the page to
// that session route.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base(
  "sidebar session row click navigates to the session route",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title: "story-row-click" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const seeded = sessions.find((s) => s.title === "story-row-click");
      if (!seeded) throw new Error("seeded session 'story-row-click' missing");
      const sessionId = seeded.id;

      await page.goto(serve.baseUrl);
      const row = page.locator('[data-testid="sidebar-session-row"]').first();
      await expect(row).toBeVisible({ timeout: 10_000 });

      await row.click();
      await expect(page).toHaveURL(
        new URL(
          `/session/${encodeURIComponent(sessionId)}`,
          serve.baseUrl,
        ).toString(),
        { timeout: 10_000 },
      );
    } finally {
      await serve.stop();
    }
  },
);
