import { test, expect, observeFor } from "./helpers/mockedTest";
import { openLiveSession } from "./helpers/liveTerminal";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// #1432: the soft keyboard shrinks the mobile terminal visually but never resizes tmux (that flashed and clipped
// scrollback). Rows latch to the no-keyboard height and the cursor stays near the viewport bottom. iOS Safari
// pads by the live occlusion; where 100dvh shrinks natively the live view adds no inset.

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

const openSession = (page: Page, handle: MockHandle) => openLiveSession(page, handle, { mobile: true, settings: null });

test.describe("Keyboard auto-resize (#1432)", () => {
  test("Safari mode: the keyboard insets the pane without resizing tmux and returns a reader to the prompt", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const baselineCount = extractResizes(handle).length;
    const baselineRows = lastResize(handle)?.rows ?? 0;
    expect(baselineRows).toBeGreaterThan(0);
    const paneHeightBefore = await paneHeight(page);

    // iOS Safari: inset by the occlusion, show fewer rows, never resize tmux.
    await setKeyboard(page, { open: true, px: 320, pwa: false });
    await expect.poll(() => paneHeight(page)).toBeLessThan(paneHeightBefore);

    expect(await paneHeight(page), "pane should shrink under the keyboard inset").toBeLessThan(paneHeightBefore);
    await observeFor(page, 800, async () => {
      expect(extractResizes(handle).length, "keyboard open must not resize tmux").toBe(baselineCount);
    });

    await setKeyboard(page, { open: false, pwa: false });
    await expect.poll(() => paneHeight(page)).toBeGreaterThanOrEqual(paneHeightBefore - 2);

    expect(await paneHeight(page)).toBeGreaterThanOrEqual(paneHeightBefore - 2);
    await observeFor(page, 800, async () => {
      expect(extractResizes(handle).length, "keyboard close must not resize tmux").toBe(baselineCount);
    });

    // Opening the keyboard is an intent to type, so it returns to the prompt from a reading position.
    await page.evaluate(() => {
      const el = document.querySelector<HTMLElement>("[data-live-terminal] > div");
      if (!el) throw new Error("live terminal scroller missing");
      el.scrollTop = 0;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect(page.getByRole("button", { name: "Back to live" })).toBeVisible();

    await page.locator('textarea[aria-label="Live terminal input"]').focus();

    // The prompt is on the first row with blanks below; the target anchors the prompt, not the tail.
    await setKeyboard(page, { open: true, px: 320, pwa: false });
    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);

    const m = await page.evaluate(() => {
      const el = document.querySelector<HTMLElement>("[data-live-terminal] > div");
      const cur = el?.querySelector<HTMLElement>("[data-live-cursor]");
      if (!el || !cur) return null;
      return { cursorTop: cur.offsetTop, scrollTop: el.scrollTop, clientHeight: el.clientHeight };
    });
    expect(m, "live cursor is rendered").not.toBeNull();
    expect(m!.cursorTop, "cursor is not above the viewport").toBeGreaterThanOrEqual(m!.scrollTop - 2);
    expect(m!.cursorTop, "cursor is not below the viewport").toBeLessThanOrEqual(m!.scrollTop + m!.clientHeight);
    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);
  });

  test("PWA mode: dvh shrink owns the layout; no inset, no tmux resize", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const baselineCount = extractResizes(handle).length;

    // Where innerHeight shrinks, no inset and no tmux resize; the dvh shrink itself cannot be simulated.
    await setKeyboard(page, { open: true, px: 320, pwa: true });
    await observeFor(page, 800, async () => {
      expect(extractResizes(handle).length).toBe(baselineCount);
      expect(await page.locator('[data-term="agent"]').evaluate((el) => (el as HTMLElement).style.paddingBottom)).toBe(
        "",
      );
    });

    const padding = await page.evaluate(() => {
      const pane = document.querySelector<HTMLElement>('[data-term="agent"]');
      return pane?.style?.paddingBottom || "";
    });
    expect(padding, "PWA mode must not add an inset (dvh shrink owns it)").toBe("");
    expect(extractResizes(handle).length, "PWA keyboard open must not emit a tmux resize").toBe(baselineCount);
  });

  test("a live-view session starts full-size: no pinned root, no stale persisted reservation", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    // A stale reservation key from older builds must be ignored.
    await page.addInitScript(() => {
      try {
        localStorage.setItem("aoe-mobile-keyboard-reservation", "320");
      } catch {
        // ignore
      }
    });
    await openSession(page, handle);

    const layout = await page.evaluate(() => {
      const root = document.querySelector<HTMLElement>("div.h-dvh.flex.flex-col");
      const panel = document.querySelector('[data-term="agent"]');
      const padded = panel?.closest<HTMLElement>("div.flex-1.flex.flex-col");
      return {
        rootInlineHeight: root?.style?.height ?? "",
        paddingBottom: padded ? getComputedStyle(padded).paddingBottom : "",
      };
    });
    // The live view wants the natural dvh shrink; only the single-pane paired shell pins the height.
    expect(layout.rootInlineHeight, "live sessions must keep the natural 100dvh root").toBe("");
    expect(["0px", "", "auto"]).toContain(layout.paddingBottom);
    expect(extractResizes(handle).length).toBeGreaterThan(0);
  });
});
