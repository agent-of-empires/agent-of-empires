// User story (#1454): clicking a structured view session row in the sidebar lands
// focus on the composer textarea. The interesting case is re-selecting the
// session that is ALREADY active: StructuredView is keyed by sessionId so it
// does not remount, the mount-time autofocus never re-fires, and only the
// explicit focus dispatch from the select handler can refocus the composer.

import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { enableStructuredViewAndWait, waitForStructuredView } from "../../helpers/acp";

base("desktop: re-selecting the active structured view session refocuses the composer", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-focus-composer" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-focus-composer");
    if (!seeded) {
      throw new Error("seeded session 'story-focus-composer' missing");
    }
    await enableStructuredViewAndWait(serve.baseUrl, seeded.id);

    await page.goto(serve.baseUrl);
    const row = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    // First select: navigates into the structured view session and mounts the
    // composer (which autofocuses on mount).
    await row.click();
    await waitForStructuredView(page);
    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    await expect(composer).toBeFocused({ timeout: 10_000 });

    // Let the mount-autofocus reclaim timers (250ms / 700ms) settle, then
    // blur so the only thing that can refocus is the select dispatch.
    await page.waitForTimeout(1_000);
    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await expect(composer).not.toBeFocused();

    // Re-select the already-active session: no remount, so this passes
    // only because the select handler dispatches composer focus.
    await row.click();
    await expect(composer).toBeFocused({ timeout: 10_000 });
  } finally {
    await serve.stop();
  }
});

base("coarse pointer: selecting a structured view session does not focus the composer", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "story-focus-composer-coarse" }),
  });

  try {
    // Chromium emulation does not reliably flip the pointer media
    // queries, so force a touch-only profile: `(pointer: coarse)` matches
    // and `(any-pointer: fine)` does not. This drives both production
    // gates, `useIsCoarsePointer()` (suppresses the select dispatch) and
    // the composer's `detectMobileInput()` (disables its mount autofocus,
    // #1178). Every other query passes through to the real matchMedia.
    await page.addInitScript(() => {
      const orig = window.matchMedia.bind(window);
      const forced: Record<string, boolean> = {
        "(pointer: coarse)": true,
        "(any-pointer: fine)": false,
      };
      window.matchMedia = (query: string) => {
        if (query in forced) {
          return {
            matches: forced[query],
            media: query,
            onchange: null,
            addEventListener: () => {},
            removeEventListener: () => {},
            addListener: () => {},
            removeListener: () => {},
            dispatchEvent: () => false,
          } as MediaQueryList;
        }
        return orig(query);
      };
    });

    const sessions = await listSessions(serve.baseUrl);
    const seeded = sessions.find((s) => s.title === "story-focus-composer-coarse");
    if (!seeded) {
      throw new Error("seeded session 'story-focus-composer-coarse' missing");
    }
    await enableStructuredViewAndWait(serve.baseUrl, seeded.id);

    await page.goto(serve.baseUrl);
    const row = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.click();
    await waitForStructuredView(page);
    const composer = page.getByRole("textbox", {
      name: /Send a message|Queue a follow-up/i,
    });
    // Give any stray focus dispatch a beat to land, then assert the
    // composer never took focus.
    await page.waitForTimeout(1_000);
    await expect(composer).not.toBeFocused();
  } finally {
    await serve.stop();
  }
});
