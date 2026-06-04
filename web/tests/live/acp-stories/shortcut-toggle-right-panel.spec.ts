// User story: Cmd+Opt+B / Ctrl+Alt+B toggles the right panel.
//
// Both this and the D shortcut flip App.tsx's `diffCollapsed`. The
// chord binding lives at useKeyboardShortcuts.ts:60-65 and uses
// e.code === "KeyB" so it works on Mac layouts where Option+B emits
// "∫" instead of "b".

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";
const ALT = process.platform === "darwin" ? "Alt" : "Alt";

base("Cmd+Opt+B toggles the right panel", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-right-panel" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-right-panel");
    if (!seeded) throw new Error("seeded session 'story-right-panel' missing");
    const sessionId = seeded.id;
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const handle = page.locator('[data-testid="content-split-resize-handle"]');
    await expect(handle).toBeVisible({ timeout: 10_000 });

    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press(`${MOD}+${ALT}+KeyB`);
    await expect(handle).toBeHidden({ timeout: 5_000 });

    await page.keyboard.press(`${MOD}+${ALT}+KeyB`);
    await expect(handle).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
