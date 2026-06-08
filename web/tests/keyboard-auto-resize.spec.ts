import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// #1432: the mobile terminal auto-resizes as the soft keyboard opens/closes.
//
// The pane is padded by the LIVE cross-platform keyboard occlusion
// (stableFullHeight - visualViewport.height), so opening the keyboard shrinks
// the terminal and closing it grows it back. The previous design latched a
// fixed reservation and required a manual fullscreen FAB to reclaim space; that
// reservation, its localStorage seed, and the FAB are gone.
//
// The occlusion commit is DEBOUNCED in useMobileKeyboard, so each open/close
// produces a single PTY resize (a bounded couple, allowing for ResizeObserver
// noise), not one per animation frame. iOS PWA / iOS 26 Safari shrink
// innerHeight with the keyboard; App.tsx still pins the root to a measured
// pixel height so occlusion padding (not a shrinking root) is the one thing
// that moves the terminal, keeping the behavior identical across platforms.

test.use({ ...devices["iPhone 13"] });

interface ResizeMsg {
  type: "resize";
  cols: number;
  rows: number;
}

function extractResizes(handle: MockHandle): ResizeMsg[] {
  const out: ResizeMsg[] = [];
  for (const msg of handle.wsMessages) {
    const s = msg.toString("utf8");
    if (!s.startsWith("{")) continue;
    try {
      const parsed = JSON.parse(s);
      if (parsed?.type === "resize") out.push(parsed);
    } catch {
      // not json
    }
  }
  return out;
}

function lastResize(handle: MockHandle): ResizeMsg | undefined {
  const all = extractResizes(handle);
  return all[all.length - 1];
}

// Override visualViewport.height (and optionally innerHeight) to simulate
// a keyboard event. Matches the helper in mobile-keyboard.spec.ts.
async function setKeyboard(
  page: Page,
  opts: { open: boolean; px?: number; pwa?: boolean },
) {
  await page.evaluate(
    ({ open, px, pwa }) => {
      const vv = window.visualViewport;
      if (!vv) return;
      const fullH =
        (window as unknown as { __fullH?: number }).__fullH ??
        window.innerHeight;
      (window as unknown as { __fullH?: number }).__fullH = Math.max(
        fullH,
        window.innerHeight,
      );

      if (open) {
        const newVvH = fullH - px!;
        Object.defineProperty(vv, "height", {
          get: () => newVvH,
          configurable: true,
        });
        if (pwa) {
          Object.defineProperty(window, "innerHeight", {
            get: () => newVvH,
            configurable: true,
          });
        }
      } else {
        const proto = Object.getPrototypeOf(vv);
        const orig = Object.getOwnPropertyDescriptor(proto, "height");
        if (orig) Object.defineProperty(vv, "height", orig);
        const origInner = Object.getOwnPropertyDescriptor(
          Window.prototype,
          "innerHeight",
        );
        if (origInner) Object.defineProperty(window, "innerHeight", origInner);
      }
      vv.dispatchEvent(new Event("resize"));
    },
    { open: opts.open, px: opts.px ?? 320, pwa: opts.pwa ?? false },
  );
}

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page
    .locator('[data-term="agent"] .xterm')
    .waitFor({ state: "visible", timeout: 10_000 });
  await expect
    .poll(() => handle.wsMessages.length, { timeout: 5_000 })
    .toBeGreaterThan(0);
}

test.describe("Keyboard auto-resize (#1432)", () => {
  test("Safari mode: opening the keyboard shrinks the terminal, closing grows it back", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const baselineCount = extractResizes(handle).length;
    const baselineRows = lastResize(handle)?.rows ?? 0;
    expect(baselineRows).toBeGreaterThan(0);

    // Open keyboard: pane is padded by the live occlusion, so the terminal
    // shrinks. Debounce collapses the animation into a single resize (allow
    // a couple for ResizeObserver noise).
    await setKeyboard(page, { open: true, px: 320, pwa: false });
    await page.waitForTimeout(800);

    const afterOpenCount = extractResizes(handle).length;
    const afterOpenRows = lastResize(handle)?.rows ?? 0;
    const openDelta = afterOpenCount - baselineCount;
    expect(
      openDelta,
      `opening the keyboard should emit 1 resize (<=2 tolerated), got ${openDelta}`,
    ).toBeGreaterThanOrEqual(1);
    expect(openDelta).toBeLessThanOrEqual(2);
    expect(
      afterOpenRows,
      "terminal should have fewer rows while the keyboard occludes the viewport",
    ).toBeLessThan(baselineRows);

    // Close keyboard: occlusion releases to 0, the terminal grows back.
    await setKeyboard(page, { open: false, pwa: false });
    await page.waitForTimeout(800);

    const afterCloseCount = extractResizes(handle).length;
    const afterCloseRows = lastResize(handle)?.rows ?? 0;
    const closeDelta = afterCloseCount - afterOpenCount;
    expect(
      closeDelta,
      `closing the keyboard should emit 1 resize (<=2 tolerated), got ${closeDelta}`,
    ).toBeGreaterThanOrEqual(1);
    expect(closeDelta).toBeLessThanOrEqual(2);
    expect(
      afterCloseRows,
      "terminal should grow back to roughly the no-keyboard row count",
    ).toBeGreaterThan(afterOpenRows);
  });

  test("PWA mode: innerHeight shrinks with the keyboard but occlusion padding still resizes the terminal", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const baselineCount = extractResizes(handle).length;
    const baselineRows = lastResize(handle)?.rows ?? 0;
    expect(baselineRows).toBeGreaterThan(0);

    // PWA: innerHeight shrinks alongside vv.height. The old design relied on
    // keyboardHeight here, which is 0 in this mode, so nothing resized. The
    // occlusion signal is measured against the remembered full height, so it
    // is non-zero and the terminal shrinks like everywhere else.
    await setKeyboard(page, { open: true, px: 320, pwa: true });
    await page.waitForTimeout(800);

    const afterOpenCount = extractResizes(handle).length;
    const afterOpenRows = lastResize(handle)?.rows ?? 0;
    const openDelta = afterOpenCount - baselineCount;
    expect(
      openDelta,
      `PWA keyboard open should emit 1 resize (<=2 tolerated), got ${openDelta}`,
    ).toBeGreaterThanOrEqual(1);
    expect(openDelta).toBeLessThanOrEqual(2);
    expect(afterOpenRows).toBeLessThan(baselineRows);
  });

  test("App root is pinned to stableViewportHeight on mobile", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const before = await page.evaluate(() => {
      const root = document.querySelector<HTMLElement>(
        "div.h-dvh.flex.flex-col",
      );
      return {
        innerHeight: window.innerHeight,
        rootInlineHeight: root?.style?.height ?? "",
      };
    });

    // The hook latches max(innerHeight, vv.height) into stableViewportHeight
    // and App.tsx applies it as inline pixel height. Without this, 100dvh
    // shrinks on iOS PWA and the terminal pane shrinks with it.
    expect(before.rootInlineHeight).toMatch(/^\d+px$/);
    expect(parseInt(before.rootInlineHeight)).toBeGreaterThanOrEqual(
      before.innerHeight - 5,
    );
  });

  test("no persisted reservation: a closed keyboard on load starts full-size", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    // Seed the now-removed reservation key. It must be ignored: the pane
    // should not start shrunk just because a prior session latched a value.
    await page.addInitScript(() => {
      try {
        localStorage.setItem("aoe-mobile-keyboard-reservation", "320");
      } catch {
        // ignore
      }
    });
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const rootPaddingBottom = await page.evaluate(() => {
      const panel = document.querySelector('[data-term="agent"]');
      const root = panel?.closest<HTMLElement>("div.flex-1.flex.flex-col");
      return root ? getComputedStyle(root).paddingBottom : "";
    });
    // No keyboard is open, so no occlusion padding is applied regardless of
    // the stale localStorage value.
    expect(["0px", "", "auto"]).toContain(rootPaddingBottom);
  });
});
