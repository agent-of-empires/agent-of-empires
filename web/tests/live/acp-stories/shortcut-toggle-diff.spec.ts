// User story: pressing D toggles the diff (right) panel on a session.
//
// Single-key shortcut (useKeyboardShortcuts.ts:88-91) flips
// App.tsx's `diffCollapsed`. ContentSplit conditionally renders its
// drag-handle + right pane based on `collapsed`, so the resize
// handle's presence is the simplest visual signal.

import { test as base, expect } from "@playwright/test";
import {
  spawnAoeServe,
  listSessions,
  seedSessionViaAoeAdd,
} from "../../helpers/aoeServe";

// FIXME: this spec is intermittently flaky on CI — both the
// `toBeHidden` and `toBeVisible` assertions on the resize handle
// have failed on otherwise-unrelated PRs, including on `main`.
// #1494 already tried to de-flake the structured view live specs with
// `expect.poll` backoffs; this one needs a separate look (likely
// focus / re-layout timing after the Shift+D toggle). Keeping it
// in the suite via `fixme` so it stays visible but does not block
// merges.
base.fixme("D key toggles the diff panel", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-diff-toggle" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-diff-toggle");
    if (!seeded) throw new Error("seeded session 'story-diff-toggle' missing");
    const sessionId = seeded.id;

    await page.goto(
      `${serve.baseUrl}/session/${encodeURIComponent(sessionId)}`,
    );
    const handle = page.locator('[data-testid="content-split-resize-handle"]');
    await expect(handle).toBeVisible({ timeout: 10_000 });
    // Click outside the terminal so focus moves to body and the
    // input-gated D shortcut fires (capture-phase listener still wins,
    // but the gate at line 80 of useKeyboardShortcuts.ts skips inputs).
    await page.locator("body").click({ position: { x: 5, y: 5 } });

    await page.keyboard.press("Shift+D");
    await expect(handle).toBeHidden({ timeout: 5_000 });

    // Collapsing the diff panel re-layouts the content split, which
    // can shift focus back into the terminal pane via xterm.js's
    // ResizeObserver focus-restore. Without explicitly blurring +
    // verifying activeElement, the second Shift+D reaches the
    // terminal textbox and becomes a literal "D" PTY keystroke
    // instead of a shortcut. A bare body click loses to xterm under
    // CI contention (#1429); force-blur + poll activeElement until
    // it is body before pressing.
    await page.evaluate(() => {
      const ae = document.activeElement as HTMLElement | null;
      ae?.blur?.();
      document.body.focus?.();
    });
    await expect
      .poll(
        () => page.evaluate(() => document.activeElement?.tagName ?? null),
        { timeout: 5_000 },
      )
      .toBe("BODY");

    await page.keyboard.press("Shift+D");
    await expect(handle).toBeVisible({ timeout: 5_000 });
  } finally {
    await serve.stop();
  }
});
