// User story: starting a session from the web wizard with the
// "Use structured view" toggle on creates a structured view session end to end, with no
// CLI command. Locks the primary-path behavior the structured view Quickstart and
// Setup docs now promise. Closes #1841.

import { test, expect } from "@playwright/test";
import { listSessions, spawnAoeServe } from "../helpers/aoeServe";

test("wizard with Use structured view on creates a structured_view session", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    // ProjectStep: a scratch dir keeps the test self-contained, advance.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "Next" }).click();

    // SessionStep: title is auto-generated, advance.
    await expect(wizard.getByRole("heading", { name: "Name your session", exact: true })).toBeVisible({
      timeout: 10_000,
    });
    await wizard.getByRole("button", { name: "Next" }).click();

    // AgentStep: claude is the default ACP-capable agent and the structured view
    // master switch is on, so the "Use structured view" toggle is shown and
    // defaults on. The docs tell the user to leave it on; assert that,
    // then advance.
    const acpToggle = wizard.getByRole("switch", {
      name: "Use structured view",
    });
    await expect(acpToggle).toBeVisible({ timeout: 10_000 });
    await expect(acpToggle).toBeChecked();
    await wizard.getByRole("button", { name: "Next" }).click();

    // ReviewStep: launch the session.
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    // Server-side: one session exists and is persisted with structured_view
    // true, the behavior the rewritten docs describe.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    const sessions = await listSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    expect(sessions[0]!.view === "structured").toBe(true);
  } finally {
    await serve.stop();
  }
});
