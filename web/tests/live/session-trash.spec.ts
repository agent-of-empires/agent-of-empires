// Live coverage for the trash/restore flow (#2489):
//   - Right-click row → Delete → DeleteSessionDialog defaults to "Move to
//     Trash" (session.delete_to_trash is on by default).
//   - "Move to Trash" fires POST /api/sessions/:id/trash; the session stays
//     in storage with trashed_at set but leaves the active sidebar list and
//     appears under the collapsible Trash section.
//   - Restore fires POST /api/sessions/:id/restore and the row returns to
//     the active list with trashed_at cleared.
//
// The dialog's trash-vs-permanent control logic is covered by the Vitest
// suite at web/src/components/__tests__/DeleteSessionDialog.test.tsx; this
// spec covers the real round-trip against `aoe serve`.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("session trash + restore via sidebar (#2489)", () => {
  base("Move to Trash hides the row, Restore brings it back", async ({ page }, testInfo) => {
    const title = "trash-me";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const seeded = await listSessions(serve.baseUrl);
      expect(seeded).toHaveLength(1);
      const sessionId = seeded[0]!.id as string;

      await page.goto(`${serve.baseUrl}/`);

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toContainText(title, { timeout: 10_000 });
      await row.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-delete']").click();

      const dialog = page.locator("[data-testid='delete-session-dialog']");
      await expect(dialog).toBeVisible();
      // Default config has delete_to_trash enabled, so the dialog offers a
      // "Delete permanently" opt-in (left unchecked) and a bare Delete
      // trashes the session.
      await expect(dialog.locator("[data-testid='delete-session-permanent']")).toBeVisible();

      const trashPromise = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/trash`) && res.request().method() === "POST",
      );
      await dialog.getByRole("button", { name: /^Delete$/ }).click();
      const trashRes = await trashPromise;
      expect(trashRes.ok()).toBe(true);

      // The record survives with trashed_at set, but the active row is gone.
      await expect
        .poll(async () => {
          const ss = await listSessions(serve.baseUrl);
          return ss.length === 1 && ss[0]!.trashed_at != null;
        })
        .toBe(true);
      await expect(row).toHaveCount(0, { timeout: 10_000 });

      // Open the footer Trash popover and restore.
      const trashToggle = page.locator("[data-testid='sidebar-trash-toggle']");
      await expect(trashToggle).toHaveAttribute("aria-label", "Trash (1)");
      await trashToggle.click();

      const restorePromise = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/restore`) && res.request().method() === "POST",
      );
      await page.locator("[data-testid='sidebar-trash-restore']").click();
      const restoreRes = await restorePromise;
      expect(restoreRes.ok()).toBe(true);

      // Back in the active list, trashed_at cleared.
      await expect
        .poll(async () => {
          const ss = await listSessions(serve.baseUrl);
          return ss.length === 1 && ss[0]!.trashed_at == null;
        })
        .toBe(true);
      await expect(row).toContainText(title, { timeout: 10_000 });
    } finally {
      await serve.stop();
    }
  });
});
