// Live coverage for the sidebar snooze flow (#1581):
//   - Right-click a session row → context menu → "Snooze…".
//   - A modal opens with the eight TUI presets (60 / 120 / 180 / 240 /
//     300 / 360 / 1440 / 10080 minutes) matching
//     `src/tui/dialogs/snooze_duration.rs`.
//   - Picking "1 hour" PATCHes /api/sessions/{id}/snooze with { minutes: 60 },
//     sets `snoozed_until` on the server, and sinks the row into the
//     collapsible footer.
//   - Unsnooze via the context menu round-trips back to live.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("sidebar snooze via context menu (#1581)", () => {
  base("snooze preset list + 1h pick + unsnooze round-trip", async ({ page }, testInfo) => {
    const title = "snooze-target";
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
      await expect(row).toContainText(title, { timeout: 10_000 });

      // ---- Open context menu → Snooze… → modal with 8 presets ----
      await row.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-snooze']").click();

      const modal = page.locator("[data-testid='snooze-modal']");
      await expect(modal).toBeVisible();

      const presets = [60, 120, 180, 240, 300, 360, 1440, 10080];
      for (const m of presets) {
        await expect(modal.locator(`[data-testid='snooze-modal-preset-${m}']`)).toBeVisible();
      }

      // ---- Pick 1 hour ----
      const snoozePatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/snooze`) && res.request().method() === "PATCH",
      );
      const issuedAt = Date.now();
      await modal.locator("[data-testid='snooze-modal-preset-60']").click();
      const response = await snoozePatch;
      expect(response.ok()).toBe(true);
      expect(response.request().postDataJSON()).toEqual({ minutes: 60 });

      // Server reflects the snooze; `snoozed_until` lands within ~60min
      // of issue time (tolerance for clock skew + network latency).
      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            const ts = list[0]?.snoozed_until as string | undefined;
            if (!ts) return null;
            return Date.parse(ts);
          },
          { timeout: 5_000 },
        )
        .toBeGreaterThan(issuedAt + 55 * 60_000);

      // Row sinks into the collapsible "Snoozed & archived" footer.
      const sunkSection = page.locator("[data-testid='sidebar-sunk-section']");
      await expect(sunkSection).toBeVisible({ timeout: 5_000 });
      await sunkSection.locator("[data-testid='sidebar-sunk-toggle']").click();
      const snoozedRow = sunkSection.locator("[data-testid='sidebar-session-row']");
      await expect(snoozedRow).toContainText(title);
      await expect(snoozedRow.locator("[aria-label='Snoozed']")).toBeVisible();

      // Regression: a snoozed row's menu only offers Unsnooze, not
      // the contradictory Pin or Archive toggles. See #1581.
      await snoozedRow.click({ button: "right" });
      await expect(page.locator("[data-testid='sidebar-context-menu-unsnooze']")).toBeVisible();
      await expect(page.locator("[data-testid='sidebar-context-menu-pin']")).toHaveCount(0);
      await expect(page.locator("[data-testid='sidebar-context-menu-archive']")).toHaveCount(0);
      // Close the menu before clicking Unsnooze in the next step.
      await page.mouse.click(5, 5);

      // ---- Unsnooze ----
      const unsnoozePatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/snooze`) && res.request().method() === "PATCH",
      );
      await snoozedRow.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-unsnooze']").click();
      const unResponse = await unsnoozePatch;
      expect(unResponse.ok()).toBe(true);
      expect(unResponse.request().postDataJSON()).toEqual({ minutes: null });

      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.snoozed_until ?? null;
          },
          { timeout: 5_000 },
        )
        .toBeNull();
    } finally {
      await serve.stop();
    }
  });
});
