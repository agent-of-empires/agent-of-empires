// Live coverage for the sidebar rename flow:
//   - Right-click a session row → context menu → Rename → inline input.
//   - Enter commits, firing PATCH /api/sessions/:id with `{ title }`.
//   - Escape cancels the edit, no PATCH.
//   - A blank value short-circuits before any PATCH fires.
//
// The contract under test lives in `SessionRow.commitRename` in
// `web/src/components/WorkspaceSidebar.tsx`. The PATCH handler is
// `update_session` in `src/server/api/sessions.rs`; live coverage
// catches a wire-format drift on either side that mocked specs miss.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

base.describe("session rename via sidebar context menu (#1220)", () => {
  base(
    "Enter commits the new title and round-trips through PATCH /api/sessions/:id",
    async ({ page }, testInfo) => {
      const original = "rename-source";
      const updated = "rename-target";
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSessionViaAoeAdd({ title: original }),
      });

      try {
        const sessions = await listSessions(serve.baseUrl);
        expect(sessions).toHaveLength(1);
        const sessionId = sessions[0]!.id as string;

        await page.goto(`${serve.baseUrl}/`);

        const row = page.locator("[data-testid='sidebar-session-row']");
        // Live specs run with `workers: 4`, so the first row paint can
        // lag cold. Bump the wait above the 5s assertion default.
        await expect(row).toHaveCount(1, { timeout: 10_000 });
        await expect(row).toContainText(original, { timeout: 10_000 });

        await row.click({ button: "right" });
        const menu = page.locator("[data-testid='sidebar-context-menu']");
        await expect(menu).toBeVisible();

        const patchPromise = page.waitForResponse(
          (res) =>
            res.url().endsWith(`/api/sessions/${sessionId}`) &&
            res.request().method() === "PATCH",
        );

        await menu
          .locator("[data-testid='sidebar-context-menu-rename']")
          .click();
        const input = page.locator("[data-testid='sidebar-rename-input']");
        await expect(input).toBeVisible();
        await input.fill(updated);
        await input.press("Enter");

        const patchRes = await patchPromise;
        expect(patchRes.ok()).toBe(true);
        expect(patchRes.request().postDataJSON()).toEqual({ title: updated });

        await expect(page.getByText(updated)).toBeVisible({ timeout: 5_000 });

        await expect
          .poll(async () => (await listSessions(serve.baseUrl))[0]?.title, {
            timeout: 5_000,
          })
          .toBe(updated);
      } finally {
        await serve.stop();
      }
    },
  );

  base(
    "Escape cancels mid-edit, no PATCH fires",
    async ({ page }, testInfo) => {
      const title = "escape-cancels";
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSessionViaAoeAdd({ title }),
      });

      try {
        await page.goto(`${serve.baseUrl}/`);

        // Spy on every PATCH-shaped request to the sessions endpoint.
        let patchSeen = false;
        await page.route("**/api/sessions/*", (route) => {
          if (route.request().method() === "PATCH") {
            patchSeen = true;
          }
          return route.continue();
        });

        const row = page.locator("[data-testid='sidebar-session-row']");
        await expect(row).toContainText(title, { timeout: 10_000 });
        await row.click({ button: "right" });
        await page
          .locator("[data-testid='sidebar-context-menu-rename']")
          .click();

        const input = page.locator("[data-testid='sidebar-rename-input']");
        await input.fill("should-not-stick");
        await input.press("Escape");

        // The input unmounts on Escape, the row reverts to its label.
        await expect(input).toBeHidden();
        await expect(row).toContainText(title);

        // Give the browser a beat to make sure no PATCH is in flight.
        await page.waitForTimeout(200);
        expect(patchSeen).toBe(false);
      } finally {
        await serve.stop();
      }
    },
  );

  base(
    "blank title is rejected by commitRename without firing PATCH",
    async ({ page }, testInfo) => {
      const title = "blank-rejected";
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSessionViaAoeAdd({ title }),
      });

      try {
        await page.goto(`${serve.baseUrl}/`);

        let patchSeen = false;
        await page.route("**/api/sessions/*", (route) => {
          if (route.request().method() === "PATCH") {
            patchSeen = true;
          }
          return route.continue();
        });

        const row = page.locator("[data-testid='sidebar-session-row']");
        await expect(row).toContainText(title, { timeout: 10_000 });
        await row.click({ button: "right" });
        await page
          .locator("[data-testid='sidebar-context-menu-rename']")
          .click();

        const input = page.locator("[data-testid='sidebar-rename-input']");
        await input.fill("   ");
        await input.press("Enter");

        // commitRename short-circuits on a blank trimmed value; the row
        // label keeps the original title and no PATCH leaves the page.
        await expect(input).toBeHidden();
        await expect(row).toContainText(title);
        await page.waitForTimeout(200);
        expect(patchSeen).toBe(false);
      } finally {
        await serve.stop();
      }
    },
  );
});
