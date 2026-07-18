// Live coverage for the session-delete flow from the sidebar:
//   - Right-click row → context menu → Delete → DeleteSessionDialog.
//   - Confirm button fires DELETE /api/workspaces with the expected
//     { session_ids, ...options } body and removes the row.
//   - Cancel button + Escape both dismiss the dialog without a DELETE.
//
// The toggle-combination matrix (delete_worktree / force / delete_branch
// / delete_sandbox permutations) is covered by the Vitest suite at
// `web/src/components/__tests__/DeleteSessionDialog.test.tsx`. This spec
// covers the round-trip: the dialog actually fires DELETE against a real
// server, and the session disappears from `GET /api/sessions`.
//
// `aoe add` without `-w` creates an attached-mode session (no managed
// worktree, not sandboxed), so DeleteSessionDialog renders the bare
// confirm form and the request body has all four flags `false`.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base.describe("session delete via sidebar context menu (#1220)", () => {
  base("Delete button fires DELETE /api/workspaces and removes the row", async ({ page }, testInfo) => {
    const title = "delete-me";
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
      // Live specs run with `workers: 4`, so the first paint can lag while
      // four `aoe serve` instances cold-start in parallel. The default 5s
      // assertion timeout is enough on a warm cache but flakes cold, so
      // bump the wait for the initial row paint here (and elsewhere in
      // this file). Subsequent assertions keep the default.
      await expect(row).toContainText(title, { timeout: 10_000 });
      await row.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-delete']").click();

      const dialog = page.locator("[data-testid='delete-session-dialog']");
      await expect(dialog).toBeVisible();

      // Trash-first is on by default (#2489), so a bare Delete trashes; tick
      // "Delete permanently" to exercise the DELETE purge path.
      await dialog.locator("[data-testid='delete-session-permanent']").click();

      const deletePromise = page.waitForResponse(
        (res) => res.url().endsWith(`/api/workspaces`) && res.request().method() === "DELETE",
      );

      // `aoe add` does not produce a managed worktree, so the dialog
      // skips the checkbox section and the confirm body is all-false. The
      // single-session workspace sends the one id as session_ids.
      await dialog.getByRole("button", { name: /^Delete$/ }).click();

      const deleteRes = await deletePromise;
      expect(deleteRes.ok()).toBe(true);
      expect(deleteRes.request().postDataJSON()).toEqual({
        session_ids: [sessionId],
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: false,
        force_delete: false,
      });

      await expect
        .poll(async () => (await listSessions(serve.baseUrl)).length, {
          timeout: 10_000,
        })
        .toBe(0);
      await expect(row).toHaveCount(0, { timeout: 10_000 });
    } finally {
      await serve.stop();
    }
  });

  base("Cancel button closes the dialog without firing DELETE", async ({ page }, testInfo) => {
    const title = "cancel-keeps-me";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      let deleteSeen = false;
      await page.route("**/api/workspaces", (route) => {
        if (route.request().method() === "DELETE") {
          deleteSeen = true;
        }
        return route.continue();
      });

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toContainText(title, { timeout: 10_000 });
      await row.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-delete']").click();

      const dialog = page.locator("[data-testid='delete-session-dialog']");
      await expect(dialog).toBeVisible();

      await dialog.getByRole("button", { name: "Cancel" }).click();
      await expect(dialog).toBeHidden();

      await page.waitForTimeout(200);
      expect(deleteSeen).toBe(false);

      // Session remains listed after the cancel.
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      expect(sessions[0]!.title).toBe(title);
    } finally {
      await serve.stop();
    }
  });

  base("Escape closes the dialog without firing DELETE", async ({ page }, testInfo) => {
    const title = "escape-keeps-me";
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      let deleteSeen = false;
      await page.route("**/api/workspaces", (route) => {
        if (route.request().method() === "DELETE") {
          deleteSeen = true;
        }
        return route.continue();
      });

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toContainText(title, { timeout: 10_000 });
      await row.click({ button: "right" });
      await page.locator("[data-testid='sidebar-context-menu-delete']").click();

      const dialog = page.locator("[data-testid='delete-session-dialog']");
      await expect(dialog).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(dialog).toBeHidden();

      await page.waitForTimeout(200);
      expect(deleteSeen).toBe(false);

      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
    } finally {
      await serve.stop();
    }
  });
});
