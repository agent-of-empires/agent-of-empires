// Live coverage for the sidebar pin flow (#1581):
//   - Right-click a session row → context menu → Pin.
//   - PATCH /api/sessions/{id}/pin lands with { pinned: true }.
//   - The row immediately renders the Pin glyph (optimistic) and
//     `pinned_at` survives a hard reload through the persistence layer.
//   - Unpin via the same menu round-trips back to `pinned_at == null`.
//
// The PATCH handler lives in `src/server/api/sessions.rs::update_session_pin`;
// the render path is in `web/src/components/WorkspaceSidebar.tsx` (the Pin
// glyph plus the `Pin/Unpin` menu entry). Live coverage catches wire-format
// drift on either side that mocked specs miss.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("sidebar pin via context menu (#1581)", () => {
  base("pin → unpin round-trip lands on the server and survives reload", async ({ page }, testInfo) => {
    const title = "pin-target";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      const sessionId = sessions[0]!.id as string;

      await page.goto(`${serve.baseUrl}/`);

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toHaveCount(1, { timeout: 10_000 });
      await expect(row).toContainText(title, { timeout: 10_000 });

      // ---- Pin ----
      await row.click({ button: "right" });
      const menu = page.locator("[data-testid='sidebar-context-menu']");
      await expect(menu).toBeVisible();

      const pinPatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/pin`) && res.request().method() === "PATCH",
      );

      await menu.locator("[data-testid='sidebar-context-menu-pin']").click();

      const pinResponse = await pinPatch;
      expect(pinResponse.ok()).toBe(true);
      expect(pinResponse.request().postDataJSON()).toEqual({ pinned: true });

      // Pin glyph appears on the row (aria-label is the stable hook).
      await expect(row.locator("[aria-label='Pinned']")).toBeVisible({
        timeout: 5_000,
      });

      // Regression: a pinned row's menu only offers Unpin, not the
      // contradictory Archive or Snooze toggles. The Pin button still
      // exists because its testid is shared with Unpin; the others
      // must be hidden entirely. See #1581. The reload below resets
      // the page so we don't need to dismiss the menu by hand.
      await row.click({ button: "right" });
      await expect(page.locator("[data-testid='sidebar-context-menu-pin']")).toContainText("Unpin");
      await expect(page.locator("[data-testid='sidebar-context-menu-archive']")).toHaveCount(0);
      await expect(page.locator("[data-testid='sidebar-context-menu-snooze']")).toHaveCount(0);

      // Server reflects the pin in the next list response.
      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.pinned_at;
          },
          { timeout: 5_000 },
        )
        .toBeTruthy();

      // Reload: pin survives the persistence layer (no in-memory only state).
      await page.reload();
      const reloadedRow = page.locator("[data-testid='sidebar-session-row']");
      await expect(reloadedRow.locator("[aria-label='Pinned']")).toBeVisible({
        timeout: 10_000,
      });

      // ---- Unpin ----
      await reloadedRow.click({ button: "right" });
      const unpinPatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/pin`) && res.request().method() === "PATCH",
      );
      await page.locator("[data-testid='sidebar-context-menu-pin']").click();
      const unpinResponse = await unpinPatch;
      expect(unpinResponse.ok()).toBe(true);
      expect(unpinResponse.request().postDataJSON()).toEqual({
        pinned: false,
      });

      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.pinned_at ?? null;
          },
          { timeout: 5_000 },
        )
        .toBeNull();
    } finally {
      await serve.stop();
    }
  });
});
