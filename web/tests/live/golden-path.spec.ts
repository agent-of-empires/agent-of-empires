// Golden-path live spec: the single most-valuable end-to-end flow.
//
// Seed a session via `aoe add` BEFORE the server boots (`seedFn`), then
// verify it shows up in the sidebar, click into it, see the terminal
// pane mount, and delete it through the API. Post-spawn `aoe add` does
// not work because `state.instances` is loaded once at boot.
//
// API-for-setup + UI-for-verify keeps the test focused on the failure
// modes Playwright is uniquely good at (rendering, routing, sidebar
// state). Wizard-driven creation is exercised by mocked specs and will
// land live in #1219.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

base("create, view, delete a session via live backend", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "golden" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    expect(sessions.length).toBeGreaterThan(0);
    const sessionId: string = sessions[0]!.id;

    await page.goto(`${serve.baseUrl}/`);
    const sessionRow = page.getByRole("link").filter({ hasText: "golden" }).first();
    await expect(sessionRow).toBeVisible({ timeout: 10_000 });

    await sessionRow.click();
    // The URL routes to the session id. Empty-state preview ("Select a
    // session to preview") is gone once we're on the session route.
    // Don't probe for "Starting session..." because ensure() can resolve
    // before Playwright catches the placeholder.
    await expect(page).toHaveURL(new RegExp(`/session/${sessionId}`), {
      timeout: 10_000,
    });

    // The web attach hides the tmux status line (the dashboard renders
    // its own chrome), so the "Ctrl+b d to detach" footer must never
    // appear in any web terminal surface.
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.waitForTimeout(1_000);
    await expect(page.locator("body")).not.toContainText("to detach");

    // Delete via API; sidebar should remove the row.
    const deleteRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}`, { method: "DELETE" });
    expect(deleteRes.ok).toBeTruthy();
    await expect(sessionRow).toBeHidden({ timeout: 10_000 });

    const after = await listSessions(serve.baseUrl);
    expect(after.find((s) => s.id === sessionId)).toBeUndefined();
  } finally {
    await serve.stop();
  }
});
