// User story: starting a session from the web wizard with the
// "Use cockpit" toggle on creates a cockpit session end to end, with no
// CLI command. Locks the primary-path behavior the cockpit Quickstart and
// Setup docs now promise. Closes #1841.

import { test, expect } from "@playwright/test";
import { listSessions, spawnAoeServe } from "../helpers/aoeServe";

test("wizard with Use cockpit on creates a cockpit_mode session", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    cockpit: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(serve.baseUrl);
    await page
      .getByRole("button", { name: "New session", exact: true })
      .first()
      .click();

    const wizard = page.locator(
      'div.fixed.inset-0.z-50:has(h1:has-text("New session"))',
    );
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    // ProjectStep: a scratch dir keeps the test self-contained, advance.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "Next" }).click();

    // SessionStep: title is auto-generated, advance.
    await expect(
      wizard.getByRole("heading", { name: "Name your session", exact: true }),
    ).toBeVisible({ timeout: 10_000 });
    await wizard.getByRole("button", { name: "Next" }).click();

    // AgentStep: claude is the default ACP-capable agent and the cockpit
    // master switch is on, so the "Use cockpit" toggle is shown and
    // defaults on. The docs tell the user to leave it on; assert that,
    // then advance.
    const cockpitToggle = wizard.getByRole("switch", { name: "Use cockpit" });
    await expect(cockpitToggle).toBeVisible({ timeout: 10_000 });
    await expect(cockpitToggle).toBeChecked();
    await wizard.getByRole("button", { name: "Next" }).click();

    // ReviewStep: launch the session.
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    // Server-side: one session exists and is persisted with cockpit_mode
    // true, the behavior the rewritten docs describe.
    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).length, {
        timeout: 15_000,
      })
      .toBeGreaterThan(0);

    const sessions = await listSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    expect(sessions[0]!.cockpit_mode).toBe(true);
  } finally {
    await serve.stop();
  }
});
