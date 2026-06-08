// Settings persistence round-trip for the theme panel.
//
// PATCH /api/settings -> GET /api/settings returns the new value; reload
// the dashboard and the value still applies. Proves the live server
// persists settings to disk (not just in-memory) and the frontend reads
// them back on every page load. The user-story covers #1217.
//
// The second test (#1510) is the user-observable form: the dashboard
// picker drives the change, the dashboard chrome repaints, and the
// new theme survives both a page reload and an `aoe serve` restart
// without any passphrase prompt firing (no-auth mode).

import { test, expect } from "../helpers/liveTest";

const SWITCH_TO = "dracula";

test("theme setting persists through PATCH + reload", async ({
  serve,
  page,
}) => {
  // Read baseline.
  const before = await fetch(`${serve.baseUrl}/api/settings`).then((r) =>
    r.json(),
  );
  const baselineTheme: string | undefined = before?.theme?.name;
  const newTheme =
    baselineTheme === "modus-vivendi" ? "default" : "modus-vivendi";

  // PATCH the theme via the same endpoint the dashboard hits.
  const patchRes = await fetch(`${serve.baseUrl}/api/settings`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      theme: { ...(before?.theme ?? {}), name: newTheme },
    }),
  });
  expect(patchRes.ok).toBeTruthy();

  // Server-side persistence: GET returns the new value immediately.
  const after = await fetch(`${serve.baseUrl}/api/settings`).then((r) =>
    r.json(),
  );
  expect(after?.theme?.name).toBe(newTheme);

  // Frontend-side persistence: reload the dashboard and the new value
  // is still what the page reads.
  await page.goto(serve.baseUrl);
  const fetched = await page.evaluate(async (url) => {
    const r = await fetch(`${url}/api/settings`);
    return r.json();
  }, serve.baseUrl);
  expect(fetched?.theme?.name).toBe(newTheme);
});

test("theme picker repaints, persists across reload and serve restart (#1510)", async ({
  serve,
  page,
}) => {
  // The theme is a global preference: the picker writes the dedicated
  // /api/theme endpoint, so persistence is read back from the global
  // settings (GET /api/settings), not a profile config.
  const globalUrl = `${serve.baseUrl}/api/settings`;

  // Drive the picker through the actual settings UI, not the REST
  // endpoint, so the regression that landed the dispatch-before-PATCH
  // bug would re-fail this test. Lands on /settings/theme directly
  // (App.tsx routes settings tabs by URL) and waits for the dropdown.
  await page.goto(`${serve.baseUrl}/settings/theme`);
  const themeSelect = page
    .locator("label", { hasText: /^Theme$/ })
    .locator("..")
    .locator("select");
  await expect(themeSelect).toBeVisible({ timeout: 10_000 });

  // Make sure the picker has the candidate option (themes fetched
  // asynchronously from /api/themes).
  await expect
    .poll(
      async () =>
        await themeSelect.evaluate(
          (sel: HTMLSelectElement, target) =>
            Array.from(sel.options).some((o) => o.value === target),
          SWITCH_TO,
        ),
      { timeout: 5_000 },
    )
    .toBe(true);

  await themeSelect.selectOption(SWITCH_TO);

  // Server-side: PATCH landed and the global config reflects the pick.
  await expect(async () => {
    const after = await fetch(globalUrl).then((r) => r.json());
    expect(after?.theme?.name).toBe(SWITCH_TO);
  }).toPass({ timeout: 5_000 });

  // Client-side repaint: the picker dispatches aoe:theme-picker-changed
  // only after the PATCH resolves, so --color-surface-900 must end up
  // at dracula's value (#282a36) without us nudging the event ourselves.
  await expect
    .poll(
      () =>
        page.evaluate(() =>
          document.documentElement.style
            .getPropertyValue("--color-surface-900")
            .trim(),
        ),
      { timeout: 5_000, intervals: [100, 200, 400] },
    )
    .toBe("#282a36");

  // No passphrase / elevation prompt fired (the daemon does not have a
  // passphrase configured in the `serve` fixture). The elevation prompt
  // is a role=dialog with the "Confirm passphrase" header (no
  // accessible name; locate by role + text).
  await expect(
    page.locator('[role="dialog"]').filter({ hasText: /Confirm passphrase/i }),
  ).toHaveCount(0);

  // Persistence across page reload.
  await page.reload();
  const afterReload = await page.evaluate(async (url) => {
    const r = await fetch(url);
    return r.json();
  }, globalUrl);
  expect(afterReload?.theme?.name).toBe(SWITCH_TO);

  // Persistence across `aoe serve` restart (process exits, config.toml
  // is what survives). The restart() helper reuses the same port.
  await serve.restart();
  const afterRestart = await fetch(globalUrl).then((r) => r.json());
  expect(afterRestart?.theme?.name).toBe(SWITCH_TO);
});
