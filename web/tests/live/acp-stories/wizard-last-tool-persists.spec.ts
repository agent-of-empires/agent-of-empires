// User story: the wizard remembers the last-picked agent across
// reloads.
//
// SessionWizard persists `data.tool` to localStorage key
// "aoe-acp-last-tool" on every submit. Reopen the wizard later and
// the agent picker pre-selects that tool, so users iterating on a
// project don't have to repeat the choice.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, seedSessionViaAoeAdd } from "../../helpers/aoeServe";

base("wizard remembers the last-picked agent after reload", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-wizard-last-tool-seed" }),
  });

  try {
    await page.goto(serve.baseUrl);
    const groupHeader = page.locator('[data-testid="sidebar-group-header"]').first();
    await groupHeader.getByRole("button", { name: /New session in /i }).click();

    // Group click opens the wizard on Review with `data.tool = claude`
    // (preselected from prefill / loadLastUsedTool fallback). Jump
    // back to the Agent step and pick a non-default tool so the
    // persistence test actually proves persistence: if save/restore
    // is broken, the wizard would fall back to "claude" on reload
    // and a claude → claude round-trip would pass falsely.
    await expect(page.getByRole("heading", { name: /Review & Launch/i })).toBeVisible({ timeout: 10_000 });
    await page
      .getByRole("button", { name: /^Agent / })
      .first()
      .click();
    await expect(page.getByRole("heading", { name: /Which AI agent\?/i })).toBeVisible({ timeout: 10_000 });
    await page
      .getByRole("button", { name: /^codex/i })
      .first()
      .click();
    await page.getByRole("button", { name: /Next/i }).click();
    await expect(page.getByRole("heading", { name: /Review & Launch/i })).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: /Launch session/i }).click();

    await expect(page.getByRole("heading", { name: /Review & Launch/i })).toBeHidden({ timeout: 20_000 });

    await page.reload();

    // saveLastUsedTool persists the chosen tool on submit success.
    // The key is `aoe-acp-last-tool` (LAST_USED_TOOL_KEY in
    // SessionWizard.tsx). localStorage alone is an implementation
    // detail; the user story is that reopening the wizard preselects
    // the tool in the rendered DOM. Reopen the wizard and assert
    // against the Review step's Agent row.
    const stored = await page.evaluate(() => localStorage.getItem("aoe-acp-last-tool"));
    expect(stored).toBe("codex");

    // Reopen via the GLOBAL "New session" button (aria-label="New
    // session"), not the per-group "New session in <name>" button.
    // The group button routes through handleCreateSession which
    // overrides prefill.tool from the latest matching session
    // (App.tsx:451), bypassing loadLastUsedTool entirely. The global
    // button passes wizardPrefill=undefined, so buildInitialData()
    // picks up the persisted tool from localStorage and the wizard
    // mounts with data.tool="codex". Open it, jump to the Agent step
    // via the step indicator dot is non-clickable, so navigate
    // through Project (pick the seeded path) → Session → Agent and
    // assert the codex tile is selected. That proves the read side
    // of loadLastUsedTool.
    await page.getByRole("button", { name: "New session", exact: true }).first().click();
    await expect(page.getByRole("heading", { name: /Project folder/i })).toBeVisible({ timeout: 10_000 });
    // Recent tile text concatenates path + timeAgo + session count, so
    // the regex must not anchor to end of string. Look for the path
    // segment followed by the session-count suffix.
    const recentProjectTile = page
      .locator("button")
      .filter({ hasText: /\/project.*session/ })
      .first();
    await expect(recentProjectTile).toBeVisible({ timeout: 5_000 });
    await recentProjectTile.click();
    await page.getByRole("button", { name: /^Next$/ }).click();
    await expect(page.getByRole("heading", { name: /Name your session/i })).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: /^Next$/ }).click();
    await expect(page.getByRole("heading", { name: /Which AI agent\?/i })).toBeVisible({ timeout: 10_000 });
    // The codex tile carries `border-brand-600` only when data.tool
    // matches. Assert that styling instead of clicking Next twice more
    // to reach Review — keeps the spec tight while still verifying the
    // read side of persistence.
    const codexTile = page.getByRole("button", { name: /^codex/i });
    await expect(codexTile).toHaveClass(/border-brand-600/, {
      timeout: 5_000,
    });
  } finally {
    await serve.stop();
  }
});
