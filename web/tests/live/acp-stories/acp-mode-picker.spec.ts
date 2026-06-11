// User story: switch the structured view's current mode via the ModePicker.
//
// ModePicker (Composer.tsx) renders a chip showing the active mode
// and opens a menu on click; selecting an entry POSTs /acp/mode
// and the fake-ACP emits current_mode_update, which the structured view
// reducer applies to flip the chip label.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { waitForStructuredView, enableStructuredViewAndWait } from "../../helpers/acp";

base("ModePicker switches the structured view mode", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-mode-picker" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mode-picker");
    if (!seeded) throw new Error("seeded session 'story-mode-picker' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    // Explicit spawn so the supervisor has an active ACP session
    // attached before setMode dispatches. Without this, /acp/mode
    // can race the implicit spawn from enable and fail silently.
    const spawnRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/spawn`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agent: "claude" }),
    });
    if (![200, 202, 409].includes(spawnRes.status)) {
      throw new Error(`structured view spawn failed: ${spawnRes.status}`);
    }

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    // ModePicker's trigger shows the current mode label. Default
    // legacy mode is "Default".
    const trigger = page
      .locator("button")
      .filter({
        has: page.locator(":scope > span", {
          hasText: /^(Default|Plan|Accept|Bypass)$/,
        }),
      })
      .first();
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    await trigger.click();

    const planMenuItem = page
      .locator('[role="menu"]')
      .getByText(/^Plan$/i)
      .first();
    await expect(planMenuItem).toBeVisible({ timeout: 5_000 });
    await planMenuItem.click();

    // The fake-ACP emits current_mode_update on session/set_mode; the
    // reducer applies that and the trigger label flips.
    await expect(trigger).toContainText(/Plan/i, { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});

// Regression for #1764: OpenCode advertises its modes ONLY via a
// `category:"mode"` config option (no ACP SessionModeState) and rejects
// any mode value outside its real list. Before the fix the picker fell
// back to claude's hardcoded taxonomy, showing a phantom "Default" that
// OpenCode rejected ("mode not found"), trapping the user. The picker
// must now read the config-option channel, show the real modes, and let
// the user switch back and forth freely.
base("ModePicker uses OpenCode's config-option modes and never traps the user", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    extraEnv: { FAKE_ACP_MODE_VIA_CONFIG_OPTION: "1" },
    seedFn: seedSessionViaAoeAdd({
      title: "story-mode-opencode",
      tool: "opencode",
    }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-mode-opencode");
    if (!seeded) throw new Error("seeded session 'story-mode-opencode' missing");
    const sessionId = seeded.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    const spawnRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/spawn`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ agent: "opencode" }),
    });
    if (![200, 202, 409].includes(spawnRes.status)) {
      throw new Error(`structured view spawn failed: ${spawnRes.status}`);
    }

    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);
    await waitForStructuredView(page);

    // The chip shows OpenCode's real default mode ("Build"), never the
    // phantom claude "Default".
    const trigger = page
      .locator("button")
      .filter({
        has: page.locator(":scope > span", { hasText: /^(Build|Plan)$/ }),
      })
      .first();
    await expect(trigger).toBeVisible({ timeout: 10_000 });
    // The chip shows OpenCode's real default mode, never the phantom
    // claude "Default". (A page-wide "Default" check would false-match
    // the reasoning-effort selector, which has its own "Default" label.)
    await expect(trigger).toContainText(/Build/i);

    // Switch to Plan via the config-option channel; the fake returns the
    // updated configOptions and the chip flips. The open mode menu must
    // not offer the phantom "Default".
    await trigger.click();
    const menu = page.locator('[role="menu"]');
    await expect(menu.getByText(/^Default$/)).toHaveCount(0);
    await menu
      .getByText(/^Plan$/i)
      .first()
      .click();
    await expect(trigger).toContainText(/Plan/i, { timeout: 10_000 });

    // Switch back to Build: this is the path that used to fail ("mode
    // not found"). It must succeed now, proving the user is not trapped.
    await trigger.click();
    await page
      .locator('[role="menu"]')
      .getByText(/^Build$/i)
      .first()
      .click();
    await expect(trigger).toContainText(/Build/i, { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
