// User story: delete the session you are currently viewing. The row
// disappears from the sidebar and the route falls back to the
// dashboard.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

base("deleting the active session falls back to /", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-delete-active" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-delete-active");
    if (!seeded) throw new Error("seeded session 'story-delete-active' missing");
    const sessionId = seeded.id;

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });

    const row = page
      .locator('[data-testid="sidebar-session-row"]')
      .filter({ hasText: "story-delete-active" })
      .first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();

    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    await expect(row).toHaveCount(0, { timeout: 10_000 });
    // After deleting the active session the route should leave the
    // /session/:id URL.
    await expect(page).not.toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });
  } finally {
    await serve.stop();
  }
});
