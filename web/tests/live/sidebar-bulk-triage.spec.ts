// Live coverage for sidebar multi-select bulk triage (#1724, #2312):
//   - Cmd/Ctrl+click selects two session rows without navigating.
//   - Right-clicking a selected row opens the bulk context menu whose
//     "Archive" fans out one PATCH per selected session (the BulkActionBar
//     popup was removed in #2312).
//   - Both sessions get `archived_at` set on the server and sink into the
//     collapsible "Snoozed & archived" footer.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("sidebar bulk archive via multi-select (#1724, #2312)", () => {
  base("selecting two rows and bulk-archiving persists both", async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: (env) => {
        seedSessionViaAoeAdd({ title: "alpha", subdir: "proj-a" })(env);
        seedSessionViaAoeAdd({ title: "beta", subdir: "proj-b" })(env);
      },
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(2);

      await page.goto(`${serve.baseUrl}/`);
      const rows = page.locator("[data-testid='sidebar-session-row']");
      await expect(rows).toHaveCount(2, { timeout: 10_000 });

      // Cmd/Ctrl+click both rows into the selection; neither click should
      // navigate to a session route.
      await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
      await rows.nth(1).click({ modifiers: ["ControlOrMeta"] });
      expect(page.url()).not.toContain("/session/");

      await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(2);

      // Right-click a selected row to open the bulk context menu, then archive.
      await rows.nth(0).click({ button: "right" });
      const menu = page.locator("[data-testid='sidebar-context-menu']");
      await expect(menu).toContainText("2 selected");
      await menu.locator("[data-testid='sidebar-context-menu-bulk-archive']").click();

      // Both sessions are archived on the server (serial fan-out).
      await expect
        .poll(
          async () => {
            const list = await listSessions(serve.baseUrl);
            return list.filter((s) => s.archived_at).length;
          },
          { timeout: 10_000 },
        )
        .toBe(2);

      // Selection clears and the rows sink into the collapsible footer.
      await expect(page.locator("[data-testid='sidebar-session-row'][data-selected]")).toHaveCount(0, {
        timeout: 5_000,
      });
      await expect(page.locator("[data-testid='sidebar-sunk-section']")).toBeVisible({ timeout: 5_000 });
    } finally {
      await serve.stop();
    }
  });
});
