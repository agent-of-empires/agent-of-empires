// Color-mode field PATCHes but does NOT dispatch the theme repaint
// event (#1405).
//
// ThemeSettings.tsx:34 early-returns from `save()` for any field other
// than `name`, so only theme.name selections drive the dashboard
// repaint. Color mode is a TUI-only setting (palette downsample for
// 256-color terminals); a refactor that drops the early-return would
// wastefully re-fetch /api/themes/<name> and dispatch
// aoe:theme-picker-changed on every color-mode toggle, repainting the
// dashboard on a TUI-only setting.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { settingsSelectByLabel } from "../../helpers/acp";

base("color-mode change PATCHes but does not dispatch theme-picker-changed", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    // Capture both theme events before the SPA mounts so any dispatch
    // is observable. addInitScript runs before any document script in
    // each page (and survives reloads).
    await page.addInitScript(() => {
      const w = window as unknown as {
        __pickerFired?: number;
        __changedFired?: number;
      };
      w.__pickerFired = 0;
      w.__changedFired = 0;
      window.addEventListener("aoe:theme-picker-changed", () => {
        w.__pickerFired = (w.__pickerFired ?? 0) + 1;
      });
      window.addEventListener("aoe:theme-changed", () => {
        w.__changedFired = (w.__changedFired ?? 0) + 1;
      });
    });

    await page.goto(`${serve.baseUrl}/settings/theme`);
    await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible({
      timeout: 10_000,
    });

    const colorMode = settingsSelectByLabel(page, "Color Mode");
    await expect(colorMode).toBeVisible({ timeout: 10_000 });

    // useResolvedTheme fires aoe:theme-changed once on mount when its
    // /api/theme/current resolves. Reset both counters AFTER mount so
    // the assertion below targets only events the color-mode pick
    // triggers.
    await expect
      .poll(
        async () =>
          await page.evaluate(
            () =>
              (window as unknown as { __changedFired?: number })
                .__changedFired ?? 0,
          ),
        { timeout: 5_000 },
      )
      .toBeGreaterThan(0);
    await page.evaluate(() => {
      const w = window as unknown as {
        __pickerFired?: number;
        __changedFired?: number;
      };
      w.__pickerFired = 0;
      w.__changedFired = 0;
    });

    const before = await page.evaluate(
      () => document.documentElement.dataset.theme,
    );
    const initialMode = await colorMode.inputValue();
    const nextMode = initialMode === "palette" ? "truecolor" : "palette";

    await colorMode.selectOption(nextMode);
    await expect(colorMode).toHaveValue(nextMode);

    // The PATCH lands; this poll reads the persisted value back via
    // GET /api/profiles/<name>/settings so a regression that drops the
    // write surfaces here, not as a missing event later.
    const profiles: Array<{ name: string; is_default?: boolean }> = await fetch(
      `${serve.baseUrl}/api/profiles`,
    ).then((r) => r.json());
    const defaultProfile =
      profiles.find((p) => p.is_default)?.name ?? profiles[0]?.name ?? "main";
    const profileUrl = `${serve.baseUrl}/api/profiles/${encodeURIComponent(defaultProfile)}/settings`;
    await expect(async () => {
      const after = await fetch(profileUrl).then((r) => r.json());
      expect(after?.theme?.color_mode).toBe(nextMode);
    }).toPass({ timeout: 5_000 });

    // The contract is "neither event fires for a color-mode change."
    // A fixed 500ms wait is enough; the dispatch would have happened
    // synchronously inside save().
    await page.waitForTimeout(500);
    const fired = await page.evaluate(() => ({
      picker:
        (window as unknown as { __pickerFired?: number }).__pickerFired ?? 0,
      changed:
        (window as unknown as { __changedFired?: number }).__changedFired ?? 0,
    }));
    expect(fired.picker).toBe(0);
    expect(fired.changed).toBe(0);

    const after = await page.evaluate(
      () => document.documentElement.dataset.theme,
    );
    expect(after).toBe(before);
  } finally {
    await serve.stop();
  }
});
