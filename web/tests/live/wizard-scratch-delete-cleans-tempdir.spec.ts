// User story: deleting a scratch session removes its scratch directory
// from disk so users do not accumulate dead `scratch/<id>/` folders.
// Closes #1324.

import { existsSync } from "node:fs";
import { test as base, expect } from "@playwright/test";
import { listSessions, spawnAoeServe } from "../helpers/aoeServe";

base("deleting a scratch session removes its scratch dir", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('[data-testid="session-wizard"]');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    const [created] = await listSessions(serve.baseUrl);
    const sessionId = created!.id as string;
    const projectPath = created!.project_path as string;
    expect(existsSync(projectPath)).toBe(true);

    // Delete via sidebar context menu.
    const row = page.locator("[data-testid='sidebar-session-row']").first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.click({ button: "right" });
    await page.locator("[data-testid='sidebar-context-menu-delete']").click();
    const dialog = page.locator("[data-testid='delete-session-dialog']");
    await expect(dialog).toBeVisible();

    // Trash-first is on by default (#2489); tick "Delete permanently" so the
    // scratch dir is actually purged.
    await dialog.locator("[data-testid='delete-session-permanent']").click();

    const deletePromise = page.waitForResponse(
      (res) => res.url().endsWith(`/api/sessions/${sessionId}`) && res.request().method() === "DELETE",
    );
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    const deleteRes = await deletePromise;
    expect(deleteRes.ok()).toBe(true);

    // The session row leaves the sidebar AND the scratch dir is gone.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 10_000,
      })
      .toBe(0);
    await expect.poll(() => existsSync(projectPath), { timeout: 5_000 }).toBe(false);
  } finally {
    await serve.stop();
  }
});
