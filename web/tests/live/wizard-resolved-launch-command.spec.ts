// User story: the new-session wizard shows the exact resolved launch
// command (post-override, post-arg-resolution) in the review step, and
// lets the user edit the command inline without duplicating the
// registry args that structured view always appends. Closes #1911.

import { test, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

test("review step shows the resolved structured view launch command and edits it without double-appending args", async ({
  page,
}, testInfo) => {
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

    // ProjectStep: scratch dir keeps the test self-contained.
    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "Next" }).click();

    // SessionStep: title auto-generated, advance.
    await expect(wizard.getByRole("heading", { name: "Name your session", exact: true })).toBeVisible({
      timeout: 10_000,
    });
    await wizard.getByRole("button", { name: "Next" }).click();

    // AgentStep: pick opencode, which has a registry arg ("acp"), so we
    // can prove the arg is shown and not duplicated on edit. Leave the
    // structured view toggle on (the default).
    await wizard.getByRole("button", { name: "opencode", exact: true }).click();
    const acpToggle = wizard.getByRole("switch", {
      name: "Use structured view",
    });
    await expect(acpToggle).toBeChecked();
    await wizard.getByRole("button", { name: "Next" }).click();

    // ReviewStep: the launch command row shows the registry command +
    // args, not the bare binary.
    const row = wizard.getByTestId("launch-command-row");
    await expect(row).toContainText("opencode acp", { timeout: 10_000 });

    // Edit the command prefix; the registry arg suffix must survive
    // exactly once (no "opencode-plannotator acp acp").
    await row.click();
    const input = wizard.getByTestId("launch-command-input");
    await input.fill("opencode-plannotator");
    await input.press("Enter");

    await expect(row).toContainText("opencode-plannotator acp", {
      timeout: 10_000,
    });
    const text = (await row.textContent()) ?? "";
    expect(text.match(/acp/g)?.length).toBe(1);
  } finally {
    await serve.stop();
  }
});

test("review step shows the structured view registry command, not the bare binary", async ({ page }, testInfo) => {
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

    await wizard.getByRole("switch", { name: "Skip project folder" }).click();
    await wizard.getByRole("button", { name: "Next" }).click();
    await expect(wizard.getByRole("heading", { name: "Name your session", exact: true })).toBeVisible({
      timeout: 10_000,
    });
    await wizard.getByRole("button", { name: "Next" }).click();

    // claude's binary is "claude" but its structured view launcher is
    // "claude-agent-acp"; the review row must show the latter, which fails
    // if the resolver regresses to binary + args.
    await wizard.getByRole("button", { name: "claude", exact: true }).click();
    await wizard.getByRole("button", { name: "Next" }).click();

    const row = wizard.getByTestId("launch-command-row");
    await expect(row).toContainText("claude-agent-acp", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
