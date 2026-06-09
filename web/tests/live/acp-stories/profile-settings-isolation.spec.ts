// User story: settings changed on one profile do not bleed into
// another profile.
//
// SettingsView saves via updateProfileSettings(selectedProfile, ...).
// On profile switch the panel re-fetches and re-renders against the
// new profile's stored values; profile B should start at the default
// regardless of edits made under profile A.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../../helpers/aoeServe";
import { openSettingsTab, settingsSelectByLabel, waitForSettingsLoaded } from "../../helpers/acp";

base("per-profile settings stay isolated across profile switches", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    await page.goto(`${serve.baseUrl}/settings`);
    await waitForSettingsLoaded(page);
    const profileSelect = settingsSelectByLabel(page, "Profile");
    const profileA = await profileSelect.inputValue();

    await openSettingsTab(page, "Tmux");
    const statusBar = settingsSelectByLabel(page, "Status bar");
    await expect(statusBar).toBeVisible({ timeout: 10_000 });

    // Change tmux status_bar on profile A to "disabled".
    await statusBar.selectOption("disabled");
    await expect(statusBar).toHaveValue("disabled");

    // Create profile B and switch to it.
    await page.getByRole("button", { name: "+ New" }).click();
    await page.getByPlaceholder("Profile name").fill("isolation-b");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await expect(profileSelect.locator("option", { hasText: "isolation-b" })).toHaveCount(1, { timeout: 5_000 });
    // Race-proofing: after switching profiles, SettingsView fires a
    // useEffect that GETs /api/profiles/<name>/settings. The assertion
    // below toHaveValue-polls the rendered select, but on slower CI
    // runners (#1383 flake) the poll can race the re-fetch and read
    // the previously-loaded profile's value within its 10s window.
    // Await the network round trip explicitly before each assertion.
    const waitForBSettings = page.waitForResponse(
      (r) => r.url().includes("/api/settings?profile=isolation-b") && r.request().method() === "GET",
    );
    await profileSelect.selectOption("isolation-b");
    await waitForBSettings;
    await openSettingsTab(page, "Tmux");

    // Profile B starts fresh; tmux status_bar should be the unset
    // default ("auto"), not "disabled".
    const statusBarB = settingsSelectByLabel(page, "Status bar");
    await expect(statusBarB).toHaveValue("auto", { timeout: 10_000 });

    // Switch back to profile A; the value set above must still hold.
    const waitForASettings = page.waitForResponse(
      (r) => r.url().includes(`/api/settings?profile=${profileA}`) && r.request().method() === "GET",
    );
    await profileSelect.selectOption(profileA);
    await waitForASettings;
    await openSettingsTab(page, "Tmux");
    const statusBarA = settingsSelectByLabel(page, "Status bar");
    await expect(statusBarA).toHaveValue("disabled", { timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});
