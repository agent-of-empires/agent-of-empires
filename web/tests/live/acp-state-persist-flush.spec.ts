// Structured view state is persisted on a 2 s trailing throttle; leaving earlier must flush it.

import type { Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";
import { chunk, endTurn, openStructuredView, script, startAcpSession } from "../helpers/acp";

const REPLY = "persist-flush reply marker";

// Swallow the 2000 ms throttle timer so only the leave event can persist.
const swallowThrottleTimer = () => {
  const realSetTimeout = window.setTimeout.bind(window);
  window.setTimeout = ((handler: TimerHandler, delay?: number, ...args: unknown[]) =>
    delay === 2000 ? 0 : realSetTimeout(handler, delay, ...args)) as typeof window.setTimeout;
};

const persistedText = (page: Page, sessionId: string) =>
  page.evaluate((key) => window.localStorage.getItem(key) ?? "", `aoe:acp-state:v1:${sessionId}`);

const leaves: Record<string, (page: Page) => Promise<void>> = {
  "the page is hidden": (page) =>
    page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
      document.dispatchEvent(new Event("visibilitychange"));
    }),
  "the page is unloaded": (page) => page.evaluate(() => window.dispatchEvent(new Event("pagehide"))),
};

for (const [leave, act] of Object.entries(leaves)) {
  test(`state is persisted when ${leave} before the throttle expires`, async ({ page, spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "persist-flush",
      fakeAcpScript: script(endTurn(chunk(REPLY))),
    });
    await page.addInitScript(swallowThrottleTimer);
    await openStructuredView(page, serve, sessionId, "hello");
    await expect(page.getByText(REPLY)).toBeVisible({ timeout: 10_000 });

    // The reply must be newer than the last throttled write, or the flush is not what persists it.
    expect(await persistedText(page, sessionId)).not.toContain(REPLY);
    await act(page);
    expect(await persistedText(page, sessionId)).toContain(REPLY);
  });
}
