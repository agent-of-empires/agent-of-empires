// A failed PATCH must NOT silently change dataset.theme or fire the
// repaint event (#1405, originally tracked in #1510).
//
// Before the fix, ThemeSettings.save() dispatched
// aoe:theme-picker-changed immediately, then awaited the PATCH; the
// dashboard repainted to the user's pick even when the server
// rejected the write. On reload the page snapped back to the
// persisted value and the user saw a silent revert. The fix at
// ThemeSettings.tsx:34-37 gates the dispatch on `await result === true`.
//
// This spec spawns a real `aoe serve`, then intercepts
// `PATCH /api/profiles/<name>/settings` from the browser side via
// `page.route()` so the response is a 500 even though the rest of
// the surface is real. Locks: dataset.theme does NOT change, and the
// aoe:theme-picker-changed event never fires.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { settingsSelectByLabel } from "../../helpers/acp";

base("PATCH 500 does not silently change dataset.theme or dispatch picker event", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.addInitScript(() => {
      const w = window as unknown as { __pickerFired?: number };
      w.__pickerFired = 0;
      window.addEventListener("aoe:theme-picker-changed", () => {
        w.__pickerFired = (w.__pickerFired ?? 0) + 1;
      });
    });

    // Reject the settings PATCH at the browser boundary. The rest of
    // the surface (themes list, current theme, GETs against the
    // profile) stays on the real serve so the rendering paths behave
    // like production. Only the write path 500s.
    await page.route(/.*\/api\/profiles\/[^/]+\/settings$/, (route) => {
      if (route.request().method() === "PATCH") {
        return route.fulfill({ status: 500, body: "boom" });
      }
      return route.continue();
    });

    await page.goto(`${serve.baseUrl}/settings/theme`);
    await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible({
      timeout: 10_000,
    });

    const themeSelect = settingsSelectByLabel(page, "Theme");
    await expect(themeSelect).toBeVisible({ timeout: 10_000 });
    await expect
      .poll(async () => themeSelect.locator("option").count(), {
        timeout: 10_000,
      })
      .toBeGreaterThan(1);

    // useResolvedTheme's mount-time apply runs before we pick. Wait
    // for it to settle, then take the baseline and reset the picker
    // counter so the assertion below targets only the pick attempt.
    await expect
      .poll(
        async () =>
          await page.evaluate(
            () => document.documentElement.dataset.theme ?? "",
          ),
        { timeout: 5_000 },
      )
      .not.toBe("");
    await page.evaluate(() => {
      (window as unknown as { __pickerFired?: number }).__pickerFired = 0;
    });
    const datasetBefore = await page.evaluate(
      () => document.documentElement.dataset.theme,
    );
    const surfaceBefore = await page.evaluate(() =>
      document.documentElement.style
        .getPropertyValue("--color-surface-900")
        .trim(),
    );

    const currentValue = await themeSelect.inputValue();
    const optionValues = await themeSelect
      .locator("option")
      .evaluateAll((els) => (els as HTMLOptionElement[]).map((o) => o.value));
    const next = optionValues.find((v) => v && v !== currentValue);
    expect(next, "themes list must include at least two options").toBeDefined();

    await themeSelect.selectOption(next!);

    // Wait long enough that the PATCH round-trip would have settled.
    // The save() awaits the PATCH; on the 500 it returns false and
    // never dispatches the picker event. A short fixed wait is
    // enough here since the contract is "never fires".
    await page.waitForTimeout(600);

    const picker = await page.evaluate(
      () =>
        (window as unknown as { __pickerFired?: number }).__pickerFired ?? 0,
    );
    expect(picker).toBe(0);

    const datasetAfter = await page.evaluate(
      () => document.documentElement.dataset.theme,
    );
    const surfaceAfter = await page.evaluate(() =>
      document.documentElement.style
        .getPropertyValue("--color-surface-900")
        .trim(),
    );
    expect(datasetAfter).toBe(datasetBefore);
    expect(surfaceAfter).toBe(surfaceBefore);
  } finally {
    await serve.stop();
  }
});
