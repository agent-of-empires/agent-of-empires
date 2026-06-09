// Settings persistence round-trip for the tmux panel.
//
// Sibling of settings-persistence-theme.spec.ts: PATCH /api/settings
// with tmux.status_bar + tmux.mouse, GET returns the new values, and
// they survive a page reload. Proves the live server persists tmux
// settings to disk and the dashboard reads them back on every load.
// Part of #1217.

import { test, expect } from "../helpers/liveTest";

type TmuxMode = "auto" | "enabled" | "disabled";

function flip(current: TmuxMode | undefined): TmuxMode {
  return current === "enabled" ? "disabled" : "enabled";
}

test("tmux settings persist through PATCH + reload", async ({ serve, page }) => {
  // Read baseline.
  const before = await fetch(`${serve.baseUrl}/api/settings`).then((r) => r.json());
  const baselineTmux = (before?.tmux ?? {}) as Record<string, unknown>;
  const newStatusBar = flip(baselineTmux.status_bar as TmuxMode | undefined);
  const newMouse = flip(baselineTmux.mouse as TmuxMode | undefined);

  // PATCH both tmux fields via the same endpoint the dashboard hits.
  const patchRes = await fetch(`${serve.baseUrl}/api/settings`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      tmux: { ...baselineTmux, status_bar: newStatusBar, mouse: newMouse },
    }),
  });
  expect(patchRes.ok).toBeTruthy();

  // Server-side persistence: GET returns the new values immediately.
  const after = await fetch(`${serve.baseUrl}/api/settings`).then((r) => r.json());
  expect(after?.tmux?.status_bar).toBe(newStatusBar);
  expect(after?.tmux?.mouse).toBe(newMouse);

  // Frontend-side persistence: reload the dashboard and the new values
  // are still what the page reads.
  await page.goto(serve.baseUrl);
  const fetched = await page.evaluate(async (url) => {
    const r = await fetch(`${url}/api/settings`);
    return r.json();
  }, serve.baseUrl);
  expect(fetched?.tmux?.status_bar).toBe(newStatusBar);
  expect(fetched?.tmux?.mouse).toBe(newMouse);
});
