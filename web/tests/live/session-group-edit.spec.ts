// Live coverage for the sidebar group-edit flow (#1726):
//   - Right-click a session row -> context menu -> Edit group -> modal.
//   - Save fires PATCH /api/sessions/:id/group with `{ group }`.
//   - Assigning a brand-new group path creates the group (it shows up in
//     GET /api/groups) and the session joins it; the change persists.
//   - Clearing the field commits `{ group: "" }`, ungrouping the session.
//
// The UI contract under test lives in `SessionGroupModal`
// (web/src/components/SessionGroupModal.tsx); the PATCH handler is
// `update_session_group` in `src/server/api/sessions.rs`. Live coverage
// catches wire-format drift on either side that a mocked spec would miss.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../helpers/aoeServe";

base.describe("session group edit via sidebar context menu (#1726)", () => {
  base(
    "Save commits a new group path, creating the group and round-tripping through PATCH",
    async ({ page }, testInfo) => {
      const title = "group-edit-new";
      const newGroup = "team/alpha";
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
        expect(sessions[0]!.group_path).toBe("");

        await page.goto(`${serve.baseUrl}/`);

        const row = page.locator("[data-testid='sidebar-session-row']");
        await expect(row).toHaveCount(1, { timeout: 10_000 });
        await expect(row).toContainText(title, { timeout: 10_000 });

        await row.click({ button: "right" });
        const menu = page.locator("[data-testid='sidebar-context-menu']");
        await expect(menu).toBeVisible();

        const patchPromise = page.waitForResponse(
          (res) =>
            res.url().endsWith(`/api/sessions/${sessionId}/group`) &&
            res.request().method() === "PATCH",
        );

        await menu
          .locator("[data-testid='sidebar-context-menu-edit-group']")
          .click();
        const modal = page.locator("[data-testid='session-group-modal']");
        await expect(modal).toBeVisible();
        const input = modal.locator(
          "[data-testid='session-group-modal-input']",
        );
        await input.fill(newGroup);
        await modal.locator("[data-testid='session-group-modal-save']").click();

        const patchRes = await patchPromise;
        expect(patchRes.ok()).toBe(true);
        expect(patchRes.request().postDataJSON()).toEqual({ group: newGroup });
        await expect(modal).toBeHidden();

        // The session persists under the new group...
        await expect
          .poll(
            async () => (await listSessions(serve.baseUrl))[0]?.group_path,
            {
              timeout: 5_000,
            },
          )
          .toBe(newGroup);

        // ...and the group now exists in the derived group list.
        const groupsRes = await fetch(`${serve.baseUrl}/api/groups`);
        const groups = (await groupsRes.json()) as Array<{ path: string }>;
        expect(groups.map((g) => g.path)).toContain(newGroup);
      } finally {
        await serve.stop();
      }
    },
  );

  base(
    "clearing the field commits an empty group, ungrouping the session",
    async ({ page }, testInfo) => {
      const title = "group-edit-clear";
      const startGroup = "team/beta";
      const serve = await spawnAoeServe({
        authMode: "none",
        workerIndex: testInfo.workerIndex,
        parallelIndex: testInfo.parallelIndex,
        seedFn: seedSessionViaAoeAdd({ title }),
      });

      try {
        const sessions = await listSessions(serve.baseUrl);
        const sessionId = sessions[0]!.id as string;

        // Put the session into a group up front via the same endpoint, so
        // the test starts from a grouped state. This mutates the running
        // server's in-memory state (unlike a pre-boot `aoe add`).
        const setRes = await fetch(
          `${serve.baseUrl}/api/sessions/${sessionId}/group`,
          {
            method: "PATCH",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ group: startGroup }),
          },
        );
        expect(setRes.ok).toBe(true);

        await page.goto(`${serve.baseUrl}/`);

        const row = page.locator("[data-testid='sidebar-session-row']");
        await expect(row).toContainText(title, { timeout: 10_000 });

        await row.click({ button: "right" });
        const menu = page.locator("[data-testid='sidebar-context-menu']");
        await expect(menu).toBeVisible();

        const patchPromise = page.waitForResponse(
          (res) =>
            res.url().endsWith(`/api/sessions/${sessionId}/group`) &&
            res.request().method() === "PATCH",
        );

        await menu
          .locator("[data-testid='sidebar-context-menu-edit-group']")
          .click();
        const modal = page.locator("[data-testid='session-group-modal']");
        await expect(modal).toBeVisible();
        const input = modal.locator(
          "[data-testid='session-group-modal-input']",
        );
        // The input is prefilled with the current group; clear it.
        await expect(input).toHaveValue(startGroup);
        await input.fill("");
        await modal.locator("[data-testid='session-group-modal-save']").click();

        const patchRes = await patchPromise;
        expect(patchRes.ok()).toBe(true);
        expect(patchRes.request().postDataJSON()).toEqual({ group: "" });
        await expect(modal).toBeHidden();

        await expect
          .poll(
            async () => (await listSessions(serve.baseUrl))[0]?.group_path,
            {
              timeout: 5_000,
            },
          )
          .toBe("");
      } finally {
        await serve.stop();
      }
    },
  );
});
