// Plugin management round-trip against a real backend (#268 / #2090).
//
// The vitest contract test mocks the API; this drives the rendered toggle in
// the browser and asserts it reaches the live registry and survives a reload.
// Uses the always-present builtin aoe.web so the test needs no install (no
// network, fully deterministic). Toggling it at runtime only rewrites config
// and reloads the in-process registry; the serve gate is startup-only, so the
// already-running daemon keeps serving and the test restores it at the end.

import { test, expect } from "../helpers/liveTest";

type PluginInfo = { id: string; enabled: boolean };

async function webEnabled(baseUrl: string): Promise<boolean> {
  const data: { plugins: PluginInfo[] } = await fetch(`${baseUrl}/api/plugins`).then((r) => r.json());
  const web = data.plugins.find((p) => p.id === "aoe.web");
  expect(web, "aoe.web must be present in the live registry").toBeTruthy();
  return web!.enabled;
}

test("disabling a builtin plugin persists to the backend and survives a reload", async ({ serve, page }) => {
  expect(await webEnabled(serve.baseUrl)).toBe(true);

  await page.goto(`${serve.baseUrl}/settings/plugins`);

  const toggle = page.getByLabel("Enable Web Dashboard");
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
      expect(await webEnabled(serve.baseUrl)).toBe(false);
    }).toPass({ timeout: 5_000 });

    // Frontend-side: the persisted state reads back after a reload.
    await page.reload();
    const toggleAfter = page.getByLabel("Enable Web Dashboard");
    await expect(toggleAfter).not.toBeChecked({ timeout: 10_000 });
  } finally {
    // Always restore aoe.web so a failed assertion above cannot leak the
    // disabled state into later live tests sharing this backend.
    if (!(await webEnabled(serve.baseUrl))) {
      const restore = page.getByLabel("Enable Web Dashboard");
      if (!(await restore.isChecked())) {
        await restore.click();
      }
      await expect(async () => {
        expect(await webEnabled(serve.baseUrl)).toBe(true);
      }).toPass({ timeout: 5_000 });
    }
  }
});
