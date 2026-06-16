// User story: launching a scratch session from the wizard creates a
// real session on the server with `scratch: true` and a `project_path`
// under the app data dir's scratch root. Closes #1324.

import { basename, dirname } from "node:path";
import { test as base, expect } from "@playwright/test";
import { listSessions, spawnAoeServe } from "../helpers/aoeServe";

base("scratch happy path: launch creates a scratch-dir session", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    // Single screen: enable scratch. The scratch callout confirms the mode
    // is armed (title auto-generates, claude is the default agent); Launch.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await expect(wizard.getByText("Scratch session")).toBeVisible({ timeout: 10_000 });
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    // Server-side: a session exists, marked scratch, with a project_path
    // whose parent directory basename is "scratch" (the harness isolates
    // the app dir under a per-worker temp tree, so we assert structure
    // rather than absolute location).
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    const sessions = await listSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    const session = sessions[0]!;
    expect(session.scratch).toBe(true);
    // Walk the path with the node:path helpers so this works on
    // Windows (`C:\foo\scratch\<id>`) as well as POSIX. The assertion
    // is "the parent dir is named scratch", expressed cross-platform.
    const projectPath = session.project_path as string;
    expect(basename(dirname(projectPath))).toBe("scratch");
  } finally {
    await serve.stop();
  }
});
