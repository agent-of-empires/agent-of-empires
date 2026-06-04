// User story: Cmd/Ctrl+` moves focus to the paired terminal panel.
//
// The chord lives in useKeyboardShortcuts.ts:50-54 and the handler in
// App.tsx:516 moves focus to the data-term="paired" panel; if the
// right panel is collapsed, it expands first.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

base("Cmd/Ctrl+` activates the paired terminal panel", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-terminal-focus" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-terminal-focus");
    if (!seeded) throw new Error("seeded session 'story-terminal-focus' missing");
    const sessionId = seeded.id;
    await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`);

    const handle = page.locator('[data-testid="content-split-resize-handle"]');
    await expect(handle).toBeVisible({ timeout: 10_000 });

    // The paired terminal panel mounts inside the right pane and is
    // tagged with data-term="paired"; clicking Cmd/Ctrl+` should
    // surface it as the active focus owner. Wait for both terminal
    // PTY WebSockets (main session + paired right-pane) to be `open`
    // before pressing the chord; while either side shows the
    // `Reconnecting…` banner, the keyboard handler races the WS open
    // and focus never lands on the connecting terminal.
    const paired = page.locator('[data-term="paired"]').first();
    await expect(paired).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(/Reconnecting/i)).toBeHidden({
      timeout: 15_000,
    });

    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press(`${MOD}+Backquote`);

    // Visibility alone can pass even if the chord no-ops because the
    // right pane is already mounted. Confirm focus actually lives
    // inside the paired panel after the chord; that's the real signal.
    await expect
      .poll(
        async () =>
          await page.evaluate(() => {
            const el = document.querySelector('[data-term="paired"]');
            const active = document.activeElement;
            return !!el && !!active && el.contains(active);
          }),
        { timeout: 10_000 },
      )
      .toBe(true);
  } finally {
    await serve.stop();
  }
});
