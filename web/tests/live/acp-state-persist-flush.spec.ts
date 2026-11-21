// Structured view state is persisted on a 2 s trailing throttle; leaving earlier must flush it.

import type { Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";
import { chunk, endTurn, openStructuredView, script, startAcpSession } from "../helpers/acp";

const REPLY = "persist-flush reply marker";

const persistedText = (page: Page, sessionId: string) =>
  page.evaluate((key) => window.localStorage.getItem(key) ?? "", `aoe:acp-state:v1:${sessionId}`);

const leaves: Record<string, (page: Page) => Promise<void>> = {
  "the page is hidden": (page) =>
    page.evaluate(() => {
      Object.defineProperty(document, "visibilityState", { configurable: true, get: () => "hidden" });
      document.dispatchEvent(new Event("visibilitychange"));
    }),
  "the page is unloaded": (page) => page.evaluate(() => window.dispatchEvent(new Event("pagehide"))),
  "the session view unmounts": (page) =>
    page.evaluate(() => {
      window.history.pushState({}, "", "/settings");
      window.dispatchEvent(new PopStateEvent("popstate"));
    }),
};

for (const [leave, act] of Object.entries(leaves)) {
  test(`state is persisted when ${leave} before the throttle expires`, async ({ page, spawnServe }) => {
    const { serve, sessionId } = await startAcpSession(spawnServe, {
      title: "persist-flush",
      fakeAcpScript: script(endTurn(chunk(REPLY))),
    });
    await openStructuredView(page, serve, sessionId, "hello");
    await expect(page.getByText(REPLY)).toBeVisible({ timeout: 10_000 });

    // No wait: the reply is newer than the last throttled write.
    await act(page);
    expect(await persistedText(page, sessionId)).toContain(REPLY);
  });
}
