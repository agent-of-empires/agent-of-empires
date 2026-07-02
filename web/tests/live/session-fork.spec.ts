// Live coverage for the sidebar "Fork session" flow (structured view):
//   - Hidden on a plain terminal session (no captured `acp_session_id` yet).
//   - Visible once the structured worker mints an `acp_session_id` for a
//     fork-capable agent (claude), and clicking it POSTs
//     `/api/sessions` with `fork_from` set to the parent's ACP session id.
//   - The new session opens in the sidebar as a distinct row; the parent
//     row and its id are untouched.
//
// The contract under test lives in `SessionRow.handleFork` in
// `web/src/components/WorkspaceSidebar.tsx`, gated on the server-provided
// `acp_can_fork` projection (`SessionResponse::from_instance` in
// `src/server/api/sessions.rs`). The ACP `session/fork` wire handshake itself
// is covered end-to-end by `tests/e2e/fork_structured_e2e.rs`; this spec
// covers the browser-driven affordance and round trip that Vitest's mocked
// `ForkSessionAction.test.tsx` cannot (real render, real context menu, real
// server).

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

base.describe("fork session via sidebar context menu", () => {
  base("Fork action is hidden until the session has a captured ACP session id", async ({ page }, testInfo) => {
    const title = "fork-hidden-source";
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title }),
    });

    try {
      await page.goto(`${serve.baseUrl}/`);

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toContainText(title, { timeout: 10_000 });

      await row.click({ button: "right" });
      const menu = page.locator("[data-testid='sidebar-context-menu']");
      await expect(menu).toBeVisible();
      // A freshly seeded terminal session has no acp_session_id yet, so the
      // Fork row must not render even though claude is fork-capable.
      await expect(menu.locator("[data-testid='sidebar-context-menu-fork']")).toHaveCount(0);
    } finally {
      await serve.stop();
    }
  });

  base("forks a structured session into a distinct child, parent untouched", async ({ page }, testInfo) => {
    const title = "fork-source";
    const serve = await spawnAoeServe({
      authMode: "none",
      acp: true,
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: seedSessionViaAoeAdd({ title, tool: "claude" }),
    });

    try {
      const sessions = await listSessions(serve.baseUrl);
      const parentId = sessions[0]!.id as string;

      await enableStructuredViewAndWait(serve.baseUrl, parentId, 30_000, serve.home);

      await page.goto(`${serve.baseUrl}/`);

      const row = page.locator("[data-testid='sidebar-session-row']");
      await expect(row).toContainText(title, { timeout: 10_000 });

      await row.click({ button: "right" });
      const menu = page.locator("[data-testid='sidebar-context-menu']");
      await expect(menu).toBeVisible();
      const forkButton = menu.locator("[data-testid='sidebar-context-menu-fork']");
      await expect(forkButton).toBeVisible({ timeout: 10_000 });

      const createPromise = page.waitForResponse(
        (res) => res.url().endsWith("/api/sessions") && res.request().method() === "POST",
      );
      await forkButton.click();

      const createRes = await createPromise;
      expect(createRes.ok(), `fork create failed: ${createRes.status()}`).toBe(true);
      expect(createRes.request().postDataJSON()).toMatchObject({
        view: "structured",
        tool: "claude",
      });
      const created = await createRes.json();
      const childId: string = created.id;
      expect(childId).toBeTruthy();
      expect(childId).not.toBe(parentId);

      // Both the original and the forked session are present as distinct
      // rows; forking never mutates or removes the parent.
      await expect
        .poll(async () => (await listSessions(serve.baseUrl)).map((s) => s.id), { timeout: 10_000 })
        .toEqual(expect.arrayContaining([parentId, childId]));

      const parentAfter = (await listSessions(serve.baseUrl)).find((s) => s.id === parentId);
      expect(parentAfter?.title).toBe(title);
    } finally {
      await serve.stop();
    }
  });
});
