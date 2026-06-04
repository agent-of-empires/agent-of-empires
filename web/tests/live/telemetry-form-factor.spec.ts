// Client form-factor on the telemetry seen ping (#1883).
//
// The seen ping now carries a coarse client class so the daemon snapshot can
// tell desktop, mobile, and installed-PWA web usage apart instead of collapsing
// every client into one `web_seen` bool. This drives one `aoe serve` from two
// browser contexts, a desktop one and an emulated mobile PWA, and asserts each
// reports its own class in the `POST /api/telemetry/seen` body. The class is
// derived from media-query primitives (display-mode, pointer, viewport); the
// mobile context injects those deterministically so the transport assertion does
// not hinge on Chromium emulation quirks (the pure classifier is unit-tested in
// `web/src/lib/__tests__/formFactor.test.ts`).

import type { BrowserContext, Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";
import { spawnAoeServe } from "../helpers/aoeServe";

/** Capture every `POST /api/telemetry/seen` body a context's page sends,
 *  parsed into `{ surface, form_factor }`. */
function captureSeenPings(
  page: Page,
): Array<{ surface?: string; form_factor?: string }> {
  const pings: Array<{ surface?: string; form_factor?: string }> = [];
  page.on("request", (req) => {
    if (
      req.method() === "POST" &&
      req.url().includes("/api/telemetry/seen")
    ) {
      const body = req.postData();
      if (!body) return;
      try {
        pings.push(JSON.parse(body));
      } catch {
        // Ignore unparseable bodies; assertions only care about the
        // well-formed posts the helper emits.
      }
    }
  });
  return pings;
}

/** Force the standalone + coarse-pointer + narrow-viewport media queries so the
 *  classifier deterministically yields `mobile_pwa`, independent of how the
 *  emulated device reports them. */
async function emulateMobilePwa(ctx: BrowserContext): Promise<void> {
  await ctx.addInitScript(() => {
    const orig = window.matchMedia.bind(window);
    window.matchMedia = ((query: string) => {
      const forced: Record<string, boolean> = {
        "(display-mode: standalone)": true,
        "(pointer: coarse)": true,
        "(min-width: 768px)": false,
      };
      if (query in forced) {
        return {
          matches: forced[query],
          media: query,
          onchange: null,
          addListener: () => {},
          removeListener: () => {},
          addEventListener: () => {},
          removeEventListener: () => {},
          dispatchEvent: () => false,
        } as unknown as MediaQueryList;
      }
      return orig(query);
    }) as typeof window.matchMedia;
  });
}

test("desktop and mobile-PWA clients report distinct form-factor classes", async ({
  browser,
}, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });
  try {
    // Desktop: a wide, fine-pointer context classifies as `desktop`.
    const desktopCtx = await browser.newContext({
      viewport: { width: 1280, height: 800 },
    });
    const desktopPage = await desktopCtx.newPage();
    const desktopPings = captureSeenPings(desktopPage);
    await desktopPage.goto(serve.baseUrl);

    // Mobile PWA: a narrow, coarse-pointer, standalone context => `mobile_pwa`.
    const mobileCtx = await browser.newContext({
      viewport: { width: 390, height: 844 },
      hasTouch: true,
      isMobile: true,
    });
    await emulateMobilePwa(mobileCtx);
    const mobilePage = await mobileCtx.newPage();
    const mobilePings = captureSeenPings(mobilePage);
    await mobilePage.goto(serve.baseUrl);

    await expect
      .poll(
        () =>
          desktopPings.some(
            (p) => p.surface === "web" && p.form_factor === "desktop",
          ),
        { timeout: 10_000 },
      )
      .toBe(true);

    await expect
      .poll(
        () =>
          mobilePings.some(
            (p) => p.surface === "web" && p.form_factor === "mobile_pwa",
          ),
        { timeout: 10_000 },
      )
      .toBe(true);

    // The two classes are genuinely distinct, not one undifferentiated client.
    expect(
      desktopPings.some((p) => p.form_factor === "mobile_pwa"),
    ).toBe(false);

    await desktopCtx.close();
    await mobileCtx.close();
  } finally {
    await serve.stop();
  }
});
