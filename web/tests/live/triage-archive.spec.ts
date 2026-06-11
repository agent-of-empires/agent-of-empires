// Live coverage for the sidebar archive flow (#1581):
//   - Right-click a session row → context menu → Archive.
//   - PATCH /api/sessions/{id}/archive lands with { archived: true, kill_pane: true }.
//   - The row moves into the collapsible "Snoozed & archived" footer.
//   - `archived_at` is set on the server and survives a reload.
//   - Unarchive via the menu (after expanding the footer) round-trips back to live.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("sidebar archive via context menu (#1581)", () => {
  base("archive sinks the row into the collapsible footer and persists", async ({ page }, testInfo) => {
    const title = "archive-target";
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

      // ---- Archive ----
      await row.click({ button: "right" });
      const archivePatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/archive`) && res.request().method() === "PATCH",
      );
      await page.locator("[data-testid='sidebar-context-menu-archive']").click();
      const response = await archivePatch;
      expect(response.ok()).toBe(true);
      expect(response.request().postDataJSON()).toEqual({
        archived: true,
        kill_pane: true,
      });

      // Server reflects the archive in the next list response.
      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.archived_at;
          },
          { timeout: 5_000 },
        )
        .toBeTruthy();

      // The row is no longer in the live tier; the collapsible footer
      // appears with the archived count.
      const sunkSection = page.locator("[data-testid='sidebar-sunk-section']");
      await expect(sunkSection).toBeVisible({ timeout: 5_000 });

      // Regression for #1600: with every workspace in the repo group
      // sunk, the orphan group header must disappear from the live
      // list. The sole live session in this project was just archived,
      // so no repo group header should remain.
      await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(0);

      // Default collapsed: the archived row is not visible until the
      // footer is expanded.
      const archivedRowInFooter = sunkSection.locator("[data-testid='sidebar-session-row']");
      await expect(archivedRowInFooter).toHaveCount(0);

      // Expand and find the row.
      await sunkSection.locator("[data-testid='sidebar-sunk-toggle']").click();
      await expect(archivedRowInFooter).toContainText(title);
      await expect(archivedRowInFooter.locator("[aria-label='Archived']")).toBeVisible();

      // Regression: an archived row's menu only offers Unarchive, not
      // the contradictory Pin or Snooze toggles. See #1581.
      await archivedRowInFooter.click({ button: "right" });
      await expect(page.locator("[data-testid='sidebar-context-menu-archive']")).toContainText("Unarchive");
      await expect(page.locator("[data-testid='sidebar-context-menu-pin']")).toHaveCount(0);
      await expect(page.locator("[data-testid='sidebar-context-menu-snooze']")).toHaveCount(0);

      // Reload: archive state survives the persistence layer; the
      // footer is still default-collapsed if the per-group localStorage
      // entry was not set yet, but the row's archived_at remains.
      await page.reload();
      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.archived_at;
          },
          { timeout: 5_000 },
        )
        .toBeTruthy();

      // ---- Unarchive (after expanding the footer post-reload) ----
      const reloadedFooter = page.locator("[data-testid='sidebar-sunk-section']");
      await expect(reloadedFooter).toBeVisible({ timeout: 10_000 });

      // Use the toggle's `aria-expanded` attribute as the source of
      // truth for the footer state; the previous `count() === 0`
      // pre-check raced against hydration and could flip the toggle
      // CLOSED when localStorage had already auto-expanded it.
      const reloadedToggle = reloadedFooter.locator("[data-testid='sidebar-sunk-toggle']");
      const expanded = await reloadedToggle.getAttribute("aria-expanded");
      if (expanded !== "true") {
        await reloadedToggle.click();
      }
      const reloadedArchivedRow = reloadedFooter.locator("[data-testid='sidebar-session-row']");
      await expect(reloadedArchivedRow).toContainText(title, {
        timeout: 10_000,
      });

      const unarchivePatch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}/archive`) && res.request().method() === "PATCH",
      );
      await reloadedArchivedRow.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-archive']").click();
      const unResponse = await unarchivePatch;
      expect(unResponse.ok()).toBe(true);
      expect(unResponse.request().postDataJSON()).toEqual({
        archived: false,
        kill_pane: true,
      });

      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list[0]?.archived_at ?? null;
          },
          { timeout: 5_000 },
        )
        .toBeNull();

      // Regression for #1600: once the row is back in the live tier
      // the repo group header reappears.
      await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(1, { timeout: 5_000 });
    } finally {
      await serve.stop();
    }
  });
});
