// Theme picker and per-profile settings against a real server.

import type { Locator, Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import { openSettingsTab, settingsSelectByLabel, waitForSettingsLoaded } from "../../helpers/acp";
import { MALFORMED_CUSTOM_THEME_TOML, VALID_CUSTOM_THEME_TOML, seedCustomTheme } from "../../helpers/theme";

const datasetTheme = (page: Page) => page.evaluate(() => document.documentElement.dataset.theme);
const surface900 = (page: Page) =>
  page.evaluate(() => document.documentElement.style.getPropertyValue("--color-surface-900").trim());

async function openThemeSelect(page: Page, baseUrl: string): Promise<Locator> {
  await page.goto(`${baseUrl}/settings`);
  await openSettingsTab(page, "Theme");
  const select = settingsSelectByLabel(page, "Theme");
  await expect(select).toBeVisible({ timeout: 10_000 });
  return select;
}

async function optionOtherThanCurrent(select: Locator): Promise<string> {
  const values = await select.locator("option").evaluateAll((els) => (els as HTMLOptionElement[]).map((o) => o.value));
  const current = await select.inputValue();
  const next = values.find((v) => v && v !== current);
  expect(next, "theme select needs an option distinct from current").toBeDefined();
  return next!;
}

async function expectOption(select: Locator, value: string) {
  await expect
    .poll(
      () => select.evaluate((sel: HTMLSelectElement, v) => Array.from(sel.options).some((o) => o.value === v), value),
      { timeout: 10_000 },
    )
    .toBe(true);
}

test("a custom theme TOML appears and applies on pick; a malformed one does not break the dropdown", async ({
  page,
  spawnServe,
}) => {
  const name = "aoe-story-custom";
  const serve = await spawnServe({
    seedFn: ({ home, xdg }) => {
      seedCustomTheme(home, xdg, name, VALID_CUSTOM_THEME_TOML);
      seedCustomTheme(home, xdg, "aoe-story-malformed", MALFORMED_CUSTOM_THEME_TOML);
    },
  });
  const select = await openThemeSelect(page, serve.baseUrl);
  // Builtins populate first, so wait for the custom entry itself.
  await expectOption(select, name);
  await expectOption(select, "dracula");

  await select.selectOption(name);
  await expect(select).toHaveValue(name);
  await expect.poll(() => datasetTheme(page), { timeout: 10_000 }).toBe(name);
  // The TOML's background projects onto --color-surface-900.
  expect((await surface900(page)).toLowerCase()).toBe("#11131c");

  await select.selectOption("dracula");
  await expect(select).toHaveValue("dracula");
  await expect.poll(() => datasetTheme(page), { timeout: 10_000 }).toBe("dracula");
});

test("PATCH 500 does not silently change dataset.theme or dispatch picker event", async ({ page, serve }) => {
  await page.addInitScript(() => {
    const w = window as unknown as { __pickerFired?: number };
    w.__pickerFired = 0;
    window.addEventListener("aoe:theme-picker-changed", () => {
      w.__pickerFired = (w.__pickerFired ?? 0) + 1;
    });
  });
  // The picker writes the global /api/theme endpoint; only that write fails.
  await page.route(/.*\/api\/theme$/, (route) =>
    route.request().method() === "PATCH" ? route.fulfill({ status: 500, body: "boom" }) : route.continue(),
  );

  await page.goto(`${serve.baseUrl}/settings/theme`);
  await expect(page.getByRole("heading", { name: "Theme" })).toBeVisible({ timeout: 10_000 });
  const select = settingsSelectByLabel(page, "Theme");
  await expect(select).toBeVisible({ timeout: 10_000 });
  await expect.poll(() => select.locator("option").count(), { timeout: 10_000 }).toBeGreaterThan(1);

  // Let the mount-time apply settle before taking the baseline.
  await expect.poll(async () => (await datasetTheme(page)) ?? "", { timeout: 5_000 }).not.toBe("");
  await page.evaluate(() => {
    (window as unknown as { __pickerFired?: number }).__pickerFired = 0;
  });
  const datasetBefore = await datasetTheme(page);
  const surfaceBefore = await surface900(page);

  await select.selectOption(await optionOtherThanCurrent(select));
  await expect(page.getByText("Failed to save, please try again")).toBeVisible();
  expect(await page.evaluate(() => (window as unknown as { __pickerFired?: number }).__pickerFired ?? 0)).toBe(0);
  expect(await datasetTheme(page)).toBe(datasetBefore);
  expect(await surface900(page)).toBe(surfaceBefore);
});

test("per-profile settings stay isolated across profile switches", async ({ page, serve }) => {
  await page.goto(`${serve.baseUrl}/settings`);
  await waitForSettingsLoaded(page);
  const profileSelect = settingsSelectByLabel(page, "Profile");
  const profileA = await profileSelect.inputValue();

  await openSettingsTab(page, "Tmux");
  const statusBar = settingsSelectByLabel(page, "Status bar");
  await expect(statusBar).toBeVisible({ timeout: 10_000 });
  await statusBar.selectOption("disabled");
  await expect(statusBar).toHaveValue("disabled");

  await page.getByRole("button", { name: "+ New" }).click();
  await page.getByPlaceholder("Profile name").fill("isolation-b");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(profileSelect.locator("option", { hasText: "isolation-b" })).toHaveCount(1, { timeout: 5_000 });

  // Await each profile's settings refetch, or the assertion can read the previous profile's value.
  const switchTo = async (profile: string, expected: string) => {
    const loaded = page.waitForResponse(
      (r) => r.url().includes(`/api/settings?profile=${profile}`) && r.request().method() === "GET",
    );
    await profileSelect.selectOption(profile);
    await loaded;
    await openSettingsTab(page, "Tmux");
    await expect(settingsSelectByLabel(page, "Status bar")).toHaveValue(expected, { timeout: 10_000 });
  };
  await switchTo("isolation-b", "auto");
  await switchTo(profileA, "disabled");
});
