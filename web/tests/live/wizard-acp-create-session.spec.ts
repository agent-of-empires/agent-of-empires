// User story: starting a session from the web wizard with the
// "Use structured view" toggle on creates a structured view session end to end, with no
// CLI command. Locks the primary-path behavior the structured view Quickstart and
// Setup docs now promise. Closes #1841.

import { test, expect } from "@playwright/test";
import { listSessions, spawnAoeServe, waitForView } from "../helpers/aoeServe";
import { waitForStructuredView } from "../helpers/acp";

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

    const wizard = page.locator('[data-testid="session-wizard"]');
    await expect(wizard).toBeVisible({ timeout: 15_000 });

    // Single screen: a scratch dir keeps the test self-contained.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();

    // claude is the default ACP-capable agent and the structured view master
    // switch is on, so the "Use structured view" toggle (under More options)
    // defaults on. The docs tell the user to leave it on; assert that, then
    // launch.
    await wizard.getByRole("button", { name: "More options" }).click();
    const acpToggle = wizard.getByRole("switch", {
      name: "Use structured view",
    });
    await expect(acpToggle).toBeVisible({ timeout: 10_000 });
    await expect(acpToggle).toBeChecked();

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
    await waitForView(serve.baseUrl, sessions[0]!.id, "structured");
  } finally {
    await serve.stop();
  }
});

test("wizard auto-approve starts Codex in full-access mode", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    extraEnv: { FAKE_ACP_MODE_VIA_CONFIG_OPTION: "codex" },
  });

  try {
    await page.goto(serve.baseUrl);
    await page.getByRole("button", { name: "New session", exact: true }).first().click();

    const wizard = page.locator('[data-testid="session-wizard"]');
    await expect(wizard).toBeVisible({ timeout: 15_000 });
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "codex", exact: true }).click();
    await wizard.getByRole("button", { name: "More options" }).click();

    const autoApprove = wizard.getByRole("switch", {
      name: "Auto-approve actions",
    });
    await autoApprove.click();
    await expect(autoApprove).toBeChecked();
    await wizard.getByRole("button", { name: /Launch session/ }).click();

    await waitForStructuredView(page);
    await expect(page.getByRole("button", { name: /Agent \(full access\)/ }).first()).toBeVisible({ timeout: 15_000 });

    const sessions = await listSessions(serve.baseUrl);
    expect(sessions).toHaveLength(1);
    expect(sessions[0]!.tool).toBe("codex");
    expect(sessions[0]!.yolo_mode).toBe(true);
  } finally {
    await serve.stop();
  }
});
