// User story: switching the selected profile via the ProfileSelector
// updates which profile's settings are being shown.
//
// Switching the dropdown calls onSelect, which the parent SettingsView
// uses to re-fetch settings for that profile. Profile-only settings
// for a fresh profile match the default profile's values, but the
// select value itself flips and persists the local selection on
// successive interactions.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";

base(
  "ProfileSelector switches the selected profile",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });

    try {
      await page.goto(`${serve.baseUrl}/settings`);
      const select = page.locator("select").first();
      await expect(select).toBeVisible({ timeout: 10_000 });
      // SettingsView mounts with selectedProfile="default" then async-
      // fetches the real profile list and re-derives the selection. If
      // we read inputValue() before that fetch resolves, we capture ""
      // (no matching option) or the stale literal "default", and the
      // later selectOption(initialProfile) hangs 60s on "did not find
      // some options". Wait for the select to settle on a real option
      // before capturing the baseline.
      // Poll until the select's value is BOTH non-empty AND present in
      // the rendered option list. Checking length > 0 alone can capture
      // a value the option list hasn't received yet, making
      // selectOption(initialProfile) below hang on "did not find some
      // options" until the 60s default timeout.
      await expect
        .poll(
          async () => {
            const value = await select.inputValue();
            const optionValues = await select
              .locator("option")
              .evaluateAll((opts) =>
                opts.map((o) => (o as HTMLOptionElement).value),
              );
            return value.length > 0 && optionValues.includes(value);
          },
          { timeout: 10_000 },
        )
        .toBe(true);
      const initialProfile = await select.inputValue();

      await page.getByRole("button", { name: "+ New" }).click();
      await page.getByPlaceholder("Profile name").fill("profile-switch-alt");
      await page.getByRole("button", { name: "Create", exact: true }).click();
      await expect(
        select.locator("option", { hasText: "profile-switch-alt" }),
      ).toHaveCount(1, { timeout: 5_000 });

      await select.selectOption("profile-switch-alt");
      await expect(select).toHaveValue("profile-switch-alt");

      await select.selectOption(initialProfile);
      await expect(select).toHaveValue(initialProfile);
    } finally {
      await serve.stop();
    }
  },
);
