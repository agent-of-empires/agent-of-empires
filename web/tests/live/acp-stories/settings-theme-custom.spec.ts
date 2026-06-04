// User story: a custom theme file in `${appDir}/themes/` is offered in
// the settings dropdown and, when picked, applies its palette to the
// dashboard (#1405).
//
// `discover_custom_themes` (src/tui/styles/mod.rs:86) scans the dir at
// daemon startup; `GET /api/themes` returns the merged builtin + custom
// list. Without this spec, a regression that drops the directory scan
// (or renames the dir, or stops invoking it from `available_themes`)
// would ship green: the existing settings-theme-select spec only
// exercises builtins.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsSelectByLabel } from "../../helpers/acp";
import {
  VALID_CUSTOM_THEME_TOML,
  seedCustomTheme,
} from "../../helpers/theme";

const CUSTOM_NAME = "aoe-story-custom";

base("custom theme TOML appears in the dropdown and applies on pick", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: ({ home, xdg }) => {
      seedCustomTheme(home, xdg, CUSTOM_NAME, VALID_CUSTOM_THEME_TOML);
    },
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await openSettingsTab(page, "Theme");

    const themeSelect = settingsSelectByLabel(page, "Theme");
    await expect(themeSelect).toBeVisible({ timeout: 10_000 });

    // The dropdown populates from /api/themes; poll for the custom
    // entry rather than reading once. A non-zero option count alone is
    // not enough since builtins always populate first.
    await expect
      .poll(
        async () =>
          await themeSelect.evaluate((sel: HTMLSelectElement, target) =>
            Array.from(sel.options).some((o) => o.value === target),
          CUSTOM_NAME),
        { timeout: 10_000 },
      )
      .toBe(true);

    await themeSelect.selectOption(CUSTOM_NAME);
    await expect(themeSelect).toHaveValue(CUSTOM_NAME);

    // applyResolvedTheme runs once the PATCH resolves and the picker
    // refetches /api/themes/<custom>. The resolved payload's `name`
    // matches the custom file's stem, so dataset.theme lands on it.
    await expect
      .poll(
        async () =>
          await page.evaluate(() => document.documentElement.dataset.theme),
        { timeout: 10_000 },
      )
      .toBe(CUSTOM_NAME);

    // The custom TOML declares `background = "#11131c"`; the web
    // projection maps theme.background to --color-surface-900. If a
    // future refactor stops projecting custom theme palettes, this
    // would fall back to whatever default the server resolves.
    const surface = await page.evaluate(() =>
      document.documentElement.style
        .getPropertyValue("--color-surface-900")
        .trim(),
    );
    expect(surface.toLowerCase()).toBe("#11131c");
  } finally {
    await serve.stop();
  }
});
