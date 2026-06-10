import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// #1432: the soft keyboard shrinks the mobile terminal visually but never
// resizes tmux. Rows are latched to the no-keyboard height, so a keyboard
// cycle only shrinks the visible part of the scroller; while shrunk, the
// live-edge scroll target anchors the CURSOR near the viewport bottom, so the
// agent's prompt stays in view instead of scrolling off the top behind a tail
// of blank rows.
//
// Resizing tmux on every keyboard cycle was tried and reverted: on the
// capture+network path it flashed the pane (blank-then-redraw) and clipped
// scrollback. The pane is padded by the LIVE cross-platform keyboard occlusion
// (stableFullHeight - visualViewport.height) on iOS regular Safari, where the
// layout viewport does not shrink; on iOS PWA / iOS 26 / Android, 100dvh
// shrinks natively and the live view adds no inset of its own.

test.use({ ...devices["iPhone 13"] });

interface ResizeMsg {
  type: "resize";
  cols: number;
  rows: number;
}

function extractResizes(handle: MockHandle): ResizeMsg[] {
  const out: ResizeMsg[] = [];
  for (const msg of handle.liveMessages) {
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
async function setKeyboard(page: Page, opts: { open: boolean; px?: number; pwa?: boolean }) {
  await page.evaluate(
    ({ open, px, pwa }) => {
      const vv = window.visualViewport;
      if (!vv) return;
      const fullH = (window as unknown as { __fullH?: number }).__fullH ?? window.innerHeight;
      (window as unknown as { __fullH?: number }).__fullH = Math.max(fullH, window.innerHeight);

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
        const origInner = Object.getOwnPropertyDescriptor(Window.prototype, "innerHeight");
        if (origInner) Object.defineProperty(window, "innerHeight", origInner);
      }
      vv.dispatchEvent(new Event("resize"));
    },
    { open: opts.open, px: opts.px ?? 320, pwa: opts.pwa ?? false },
  );
}

async function paneHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.querySelector<HTMLElement>("[data-live-terminal]");
    return el?.getBoundingClientRect().height ?? 0;
  });
}

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator('[data-term="agent"] [data-live-terminal]').waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
}

test.describe("Keyboard auto-resize (#1432)", () => {
  test("Safari mode: keyboard insets the pane but never resizes tmux", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const baselineCount = extractResizes(handle).length;
    const baselineRows = lastResize(handle)?.rows ?? 0;
    expect(baselineRows).toBeGreaterThan(0);
    const paneHeightBefore = await paneHeight(page);

    // iOS regular Safari: the layout viewport does not shrink with the
    // keyboard, so the live view insets itself by the visualViewport
    // delta. The pane shrinks visually, but rows are latched to the
    // no-keyboard height, so tmux must NOT be resized: the scroller
    // pins to the live content and simply shows fewer rows.
    await setKeyboard(page, { open: true, px: 320, pwa: false });
    await page.waitForTimeout(800);

    expect(await paneHeight(page), "pane should shrink under the keyboard inset").toBeLessThan(paneHeightBefore);
    expect(extractResizes(handle).length, "keyboard open must not emit a tmux resize (rows are latched)").toBe(
      baselineCount,
    );

    // Close: inset releases, still no tmux resize.
    await setKeyboard(page, { open: false, pwa: false });
    await page.waitForTimeout(800);

    expect(await paneHeight(page)).toBeGreaterThanOrEqual(paneHeightBefore - 2);
    expect(extractResizes(handle).length, "keyboard close must not emit a tmux resize").toBe(baselineCount);
  });

  test("Safari mode: the prompt cursor stays visible in the keyboard-shrunk viewport", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    // The mock screen is a fresh-agent shape: prompt + cursor on the
    // FIRST screen row, blank rows below. With rows latched, the screen
    // is now taller than the shrunk viewport; a literal bottom pin would
    // show only the blank tail and scroll the prompt off the top (the
    // original bug). The live-edge target must anchor the cursor near
    // the viewport bottom instead.
    await setKeyboard(page, { open: true, px: 320, pwa: false });
    await page.waitForTimeout(800);

    const m = await page.evaluate(() => {
      const el = document.querySelector<HTMLElement>("[data-live-terminal] > div");
      const cur = el?.querySelector<HTMLElement>("[data-live-cursor]");
      if (!el || !cur) return null;
      return { cursorTop: cur.offsetTop, scrollTop: el.scrollTop, clientHeight: el.clientHeight };
    });
    expect(m, "live cursor is rendered").not.toBeNull();
    expect(m!.cursorTop, "cursor is not above the viewport").toBeGreaterThanOrEqual(m!.scrollTop - 2);
    expect(m!.cursorTop, "cursor is not below the viewport").toBeLessThanOrEqual(m!.scrollTop + m!.clientHeight);
  });

  test("PWA mode: dvh shrink owns the layout; no inset, no tmux resize", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const baselineCount = extractResizes(handle).length;

    // iOS PWA / iOS 26 / Android: innerHeight (and 100dvh) shrink with
    // the keyboard, so the layout shrinks natively and the live view
    // must add NO inset of its own (keyboardHeight is 0 in this mode)
    // and never resize tmux. The dvh shrink itself cannot be simulated
    // here (it tracks the real viewport, not the patched innerHeight);
    // what is testable is that the legacy machinery stays quiet.
    await setKeyboard(page, { open: true, px: 320, pwa: true });
    await page.waitForTimeout(800);

    const padding = await page.evaluate(() => {
      const pane = document.querySelector<HTMLElement>('[data-term="agent"]');
      return pane?.style?.paddingBottom || "";
    });
    expect(padding, "PWA mode must not add an inset (dvh shrink owns it)").toBe("");
    expect(extractResizes(handle).length, "PWA keyboard open must not emit a tmux resize").toBe(baselineCount);
  });

  test("App root is NOT pinned for live-view sessions (dvh shrink wanted)", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);
    await page.waitForTimeout(1000);

    const rootInlineHeight = await page.evaluate(() => {
      const root = document.querySelector<HTMLElement>("div.h-dvh.flex.flex-col");
      return root?.style?.height ?? "";
    });

    // The stableViewportHeight pin exists to stop dvh from shrinking
    // under an xterm surface (every shrink would SIGWINCH the PTY). The
    // live view has no PTY and WANTS the natural dvh shrink, so the pin
    // must stay off; only the single-pane paired shell still pins.
    expect(rootInlineHeight, "live sessions must keep the natural 100dvh root").toBe("");
    expect(extractResizes(handle).length).toBeGreaterThan(0);
  });

  test("no persisted reservation: a closed keyboard on load starts full-size", async ({ page }) => {
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
