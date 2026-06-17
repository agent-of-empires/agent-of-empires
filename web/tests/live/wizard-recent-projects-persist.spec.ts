// User story (#2141): a project I recently worked in stays in the new-session
// wizard Recent tab after its last session is deleted.
//
// The Recent list was previously derived purely from live sessions, so
// deleting the only session in a project made it vanish. This spec seeds one
// session in a project, deletes it, and asserts the project survives both in
// GET /api/recent-projects and in the wizard's Recent tab (with a zero session
// count). The merge logic itself is unit-tested in
// `web/src/components/session-wizard/__tests__/ProjectStep.merge.test.tsx`;
// this spec proves the persistence round-trip against a real server.

import { test, expect } from "@playwright/test";
import { listSessions, spawnAoeServe, seedSessionViaAoeAdd } from "../helpers/aoeServe";

test("project stays in the wizard Recent tab after its last session is deleted (#2141)", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "frontend-work", subdir: "frontend" }),
  });

  try {
    const seeded = await listSessions(serve.baseUrl);
    expect(seeded).toHaveLength(1);
    const sessionId = seeded[0]!.id as string;

    // Delete the only session in the project.
    const del = await page.request.delete(`${serve.baseUrl}/api/sessions/${sessionId}`, { data: {} });
    expect(del.ok()).toBe(true);
    await expect.poll(async () => (await listSessions(serve.baseUrl)).length, { timeout: 10_000 }).toBe(0);

    // The project persists in the recent-projects store.
    const recent = await (await page.request.get(`${serve.baseUrl}/api/recent-projects`)).json();
    const paths = (recent.projects as { path: string; display_name: string }[]).map((p) => p.display_name);
    expect(paths).toContain("frontend");

    // And it shows in the wizard Recent tab, with no live sessions.
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('[data-testid="session-wizard"]');
    await expect(wizard).toBeVisible({ timeout: 15_000 });
    await expect(wizard.getByText("frontend", { exact: true })).toBeVisible({ timeout: 10_000 });
    await expect(wizard.getByText("0 sessions")).toBeVisible();
  } finally {
    await serve.stop();
  }
});
