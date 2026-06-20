// Plugin management round-trip against a real backend (#268 / #2090).
//
// The vitest contract test mocks the API; this drives the rendered toggle in
// the browser and asserts it reaches the live registry and survives a reload.
// Uses the always-present builtin aoe.status so the test needs no install
// (no network, fully deterministic).

import { test, expect } from "../helpers/liveTest";

type PluginInfo = { id: string; enabled: boolean };

async function statusEnabled(baseUrl: string): Promise<boolean> {
  const data: { plugins: PluginInfo[] } = await fetch(`${baseUrl}/api/plugins`).then((r) => r.json());
  const status = data.plugins.find((p) => p.id === "aoe.status");
  expect(status, "aoe.status must be present in the live registry").toBeTruthy();
  return status!.enabled;
}

test("disabling a builtin plugin persists to the backend and survives a reload", async ({ serve, page }) => {
  expect(await statusEnabled(serve.baseUrl)).toBe(true);

  await page.goto(`${serve.baseUrl}/settings/plugins`);

  const toggle = page.getByLabel("Enable Agent Status Detection");
  await expect(toggle).toBeVisible({ timeout: 10_000 });
  await expect(toggle).toBeChecked();

  try {
    // The checkbox is controlled by `plugin.enabled`, which only flips after
    // the async setPluginEnabled + reload round-trip, so a plain click (not
    // uncheck, which asserts the state changed synchronously) is what models
    // a real user.
    await toggle.click();

    // Server-side: the disable reached the registry.
    await expect(async () => {
      expect(await statusEnabled(serve.baseUrl)).toBe(false);
    }).toPass({ timeout: 5_000 });

    // Frontend-side: the persisted state reads back after a reload.
    await page.reload();
    const toggleAfter = page.getByLabel("Enable Agent Status Detection");
    await expect(toggleAfter).not.toBeChecked({ timeout: 10_000 });
  } finally {
    // Always restore aoe.status so a failed assertion above cannot leak the
    // disabled state into later live tests sharing this backend.
    if (!(await statusEnabled(serve.baseUrl))) {
      const restore = page.getByLabel("Enable Agent Status Detection");
      if (!(await restore.isChecked())) {
        await restore.click();
      }
      await expect(async () => {
        expect(await statusEnabled(serve.baseUrl)).toBe(true);
      }).toPass({ timeout: 5_000 });
    }
  }
});
