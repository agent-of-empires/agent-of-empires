// User story: a malformed custom theme TOML in `${appDir}/themes/`
// does NOT break the settings dropdown; builtins still load and can be
// picked (#1405).
//
// `discover_custom_themes` (src/tui/styles/mod.rs:86) scans by file
// stem and surfaces the malformed name in /api/themes alongside the
// builtins; `load_custom_theme` (mod.rs:124) returns None on parse
// failure, so a user who selects the malformed entry gets the default
// fallback palette. The user-observable contract this spec locks is
// narrower: dropping a bad TOML in the dir must not 500 the settings
// page or wipe the builtin list. Without this, a regression that
// propagates the parse error up through `available_themes` would block
// the entire theme picker.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsSelectByLabel } from "../../helpers/acp";
import {
  MALFORMED_CUSTOM_THEME_TOML,
  seedCustomTheme,
} from "../../helpers/theme";

const MALFORMED_NAME = "aoe-story-malformed";
const SWITCH_TO = "dracula";

base(
  "malformed custom theme TOML does not break the dropdown",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
      seedFn: ({ home, xdg }) => {
        seedCustomTheme(home, xdg, MALFORMED_NAME, MALFORMED_CUSTOM_THEME_TOML);
      },
    });

    try {
      await page.goto(`${serve.baseUrl}/settings`);
      await openSettingsTab(page, "Theme");

      const themeSelect = settingsSelectByLabel(page, "Theme");
      await expect(themeSelect).toBeVisible({ timeout: 10_000 });

      // Dropdown still populates: builtins are there and can be picked.
      // We assert dracula specifically because the rest of the suite
      // uses it as the switch-target; if the builtin set ever changes,
      // pick any non-default builtin instead.
      await expect
        .poll(
          async () =>
            await themeSelect.evaluate(
              (sel: HTMLSelectElement, target) =>
                Array.from(sel.options).some((o) => o.value === target),
              SWITCH_TO,
            ),
          { timeout: 10_000 },
        )
        .toBe(true);

      // Switching to a builtin lands. The PATCH succeeds (the server
      // does not validate names server-side; it would simply resolve
      // through `resolve_theme`).
      await themeSelect.selectOption(SWITCH_TO);
      await expect(themeSelect).toHaveValue(SWITCH_TO);
      await expect
        .poll(
          async () =>
            await page.evaluate(() => document.documentElement.dataset.theme),
          { timeout: 10_000 },
        )
        .toBe(SWITCH_TO);
    } finally {
      await serve.stop();
    }
  },
);
