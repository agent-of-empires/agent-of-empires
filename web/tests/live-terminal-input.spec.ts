import { test, expect, observeFor } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, seedSettings, fireTouches, type MockHandle } from "./helpers/terminal-mocks";
import { DESKTOP, iPhone13 } from "./helpers/viewports";
import {
  expectScrollMode,
  liveContent,
  liveMatches,
  liveTexts,
  openLiveTerminal,
  pushModeFrame,
  scroller,
} from "./helpers/liveTerminal";

// A full-screen (alternate-screen) mouse agent receives forwarded mouse events
// instead of driving the capture window. These drive the real bundle in a real
// browser, where pointerCell maps to measured cells (jsdom cannot), so the byte
// assertions live here; the SGR encodings themselves are unit-tested.
test.describe("Live terminal mouse forwarding (mobile)", () => {
  test.use(iPhone13);

  const pointer = (page: Page, type: string, x: number, y: number, init: Record<string, unknown> = {}) =>
    scroller(page).dispatchEvent(type, { pointerType: "mouse", button: 0, clientX: x, clientY: y, ...init });

  const hasLegacyDown = (h: MockHandle) =>
    h.liveMessages.some((b) => b.length >= 4 && b[0] === 0x1b && b[1] === 0x5b && b[2] === 0x4d && b[3] === 0x61);

  async function swipeUp(page: Page) {
    await fireTouches(page, "touchstart", [{ x: 100, y: 300 }]);
    await fireTouches(page, "touchmove", [{ x: 100, y: 220 }]);
    await fireTouches(page, "touchend", [{ x: 100, y: 220 }]);
  }

  /** Open the session in the given screen mode and hand back the handle and scroller box. */
  async function mount(page: Page, flags: { altScreen: boolean; mouse: boolean; mouseSgr: boolean }) {
    const handle = await openLiveTerminal(page, { mobile: true });
    await pushModeFrame(handle, flags);
    await expectScrollMode(page, flags.altScreen ? "forward" : "read");
    return handle;
  }

  test("a left-button drag on a full-screen SGR-mouse app forwards press, motion, and release", async ({ page }) => {
    const handle = await mount(page, { altScreen: true, mouse: true, mouseSgr: true });
    const box = (await scroller(page).boundingBox())!;
    await pointer(page, "pointerdown", box.x + 20, box.y + 20);
    await pointer(page, "pointermove", box.x + 160, box.y + 20); // far enough to cross cells
    await pointer(page, "pointerup", box.x + 160, box.y + 20);
    // Press: SGR left button (0), `M`. Motion rides at +32. Release: lowercase `m`.
    await expect.poll(() => liveMatches(handle, /\x1b\[<0;\d+;\d+M/)).toBe(true);
    await expect.poll(() => liveMatches(handle, /\x1b\[<32;\d+;\d+M/)).toBe(true);
    await expect.poll(() => liveMatches(handle, /\x1b\[<0;\d+;\d+m/)).toBe(true);
  });

  test("a click on a bottom-aligned row reports that row to the mouse app", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });
    const lines = ["first", "second", "third", ...Array<string>(21).fill("")];
    await handle.pushLiveFrame({
      content: `${lines.join("\n")}\n`,
      rows: 24,
      history: 120,
      altScreen: true,
      mouse: true,
      mouseSgr: true,
    });
    const secondRow = page.getByText("second", { exact: true });
    await expect(secondRow).toBeVisible();
    const box = (await secondRow.boundingBox())!;

    await pointer(page, "pointerdown", box.x + 10, box.y + box.height / 2);
    await expect
      .poll(() => liveTexts(handle).find((text) => /\x1b\[<0;\d+;\d+M/.test(text)))
      .toMatch(/\x1b\[<0;\d+;2M/);
  });

  // Shift is the local-selection escape hatch; a normal-screen agent owns no
  // mouse at all. Neither path may put mouse bytes on the wire.
  test("Shift+click and a normal-screen agent forward no mouse bytes", async ({ page }) => {
    const handle = await mount(page, { altScreen: true, mouse: true, mouseSgr: true });
    const box = (await scroller(page).boundingBox())!;
    await pointer(page, "pointerdown", box.x + 30, box.y + 20, { shiftKey: true });
    await pushModeFrame(handle, { altScreen: false, mouse: true, mouseSgr: true });
    await expectScrollMode(page, "read");
    await pointer(page, "pointerdown", box.x + 30, box.y + 20);
    await pointer(page, "pointerup", box.x + 30, box.y + 20);
    await swipeUp(page);
    await observeFor(page, 300, async () => {
      expect(liveMatches(handle, /\x1b\[</)).toBe(false);
      expect(liveTexts(handle).some((s) => s.startsWith("\x1b[M"))).toBe(false);
      expect(hasLegacyDown(handle)).toBe(false);
    });
  });

  test("swipe over a full-screen SGR-mouse app forwards SGR wheel bytes", async ({ page }) => {
    const handle = await mount(page, { altScreen: true, mouse: true, mouseSgr: true });
    // touch-action: none is what keeps the drag from panning the whole page:
    // React's delegated touch listeners are passive, so the component cannot
    // preventDefault the native pan.
    await expect.poll(() => scroller(page).evaluate((el) => getComputedStyle(el).touchAction)).toBe("none");
    // A direct, non-passive listener backs this up if WebKit decided the
    // gesture's touch-action before the frame switched into forward mode.
    await expect
      .poll(() => scroller(page).evaluate((el) => el.dispatchEvent(new Event("touchmove", { cancelable: true }))))
      .toBe(false);
    await swipeUp(page);
    await expect.poll(() => liveTexts(handle).some((s) => s.includes("\x1b[<65;"))).toBe(true);

    // Downward swipe forwards wheel UP (button 64).
    await fireTouches(page, "touchstart", [{ x: 100, y: 120 }]);
    await fireTouches(page, "touchmove", [{ x: 100, y: 300 }]);
    await fireTouches(page, "touchend", [{ x: 100, y: 300 }]);
    await expect.poll(() => liveTexts(handle).some((s) => s.includes("\x1b[<64;"))).toBe(true);

    // Wheel events in all three deltaModes (px / line / page) plus a sub-notch
    // delta (no-op) and a scroll, which must NOT enter reading in forward mode.
    await scroller(page).dispatchEvent("wheel", { deltaY: 120, deltaMode: 0 });
    await scroller(page).dispatchEvent("wheel", { deltaY: 3, deltaMode: 1 });
    await scroller(page).dispatchEvent("wheel", { deltaY: 1, deltaMode: 2 });
    await scroller(page).dispatchEvent("wheel", { deltaY: 1, deltaMode: 0 });
    await scroller(page).dispatchEvent("scroll", {});
    await expect(page.getByRole("button", { name: "Back to live" })).toHaveCount(0);
  });

  test("swipe over a full-screen LEGACY-mouse app forwards X10 wheel bytes", async ({ page }) => {
    const handle = await mount(page, { altScreen: true, mouse: true, mouseSgr: false });
    await swipeUp(page);
    await expect.poll(() => hasLegacyDown(handle)).toBe(true);
    expect(liveTexts(handle).some((s) => s.includes("\x1b[<"))).toBe(false);
  });
});

// Select-to-copy over a full-screen agent, driven end to end. jsdom covers the
// hold's logic (MobileLiveTerminal.selectionHold.test.tsx); this is here because
// the bug is Selection semantics, which jsdom only simulates. The mocked suite
// is Chromium, so it says nothing about the WebKit callout it exists for.
test.describe("Live terminal selection hold (mobile)", () => {
  test.use(iPhone13);

  // A full-screen agent has no scrollback and the transcript slides up through
  // a fixed grid, so every screen row holds new text on the next frame.
  const altFrame = (n: number) => ({
    content: [`line ${n}`, `line ${n + 1}`, `line ${n + 2}`, "", "> prompt"].join("\n") + "\n",
    rows: 5,
    history: 0,
    cursor: null,
    altScreen: true,
    mouse: false,
    mouseSgr: false,
  });

  const selection = (page: Page) => page.evaluate(() => window.getSelection()?.toString() ?? "");

  // Both endpoints inside the row's text node, the way a long-press word
  // selection anchors. Anchoring on the row element instead would put them
  // outside the rewritten data and survive a repaint a real gesture cannot.
  async function selectRow(page: Page, text: string) {
    await page.evaluate((t) => {
      const row = Array.from(document.querySelectorAll("[data-live-content] > div")).find((r) => r.textContent === t);
      if (!row) throw new Error("row not rendered");
      const node = document.createTreeWalker(row, NodeFilter.SHOW_TEXT).nextNode() as Text;
      const range = document.createRange();
      range.setStart(node, 0);
      range.setEnd(node, node.data.length);
      const sel = window.getSelection()!;
      sel.removeAllRanges();
      sel.addRange(range);
    }, text);
  }

  test("a selection over a full-screen agent survives its repaints", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });

    await handle.pushLiveFrame(altFrame(1));
    await expect.poll(() => liveContent(page).textContent()).toContain("line 1");

    await selectRow(page, "line 2");
    expect(await selection(page)).toBe("line 2");

    await handle.pushLiveFrame(altFrame(2));
    await handle.pushLiveFrame(altFrame(3));
    expect(await selection(page)).toBe("line 2");
    await expect(liveContent(page)).toContainText("line 1");

    // Letting go releases the hold and the view catches up to the live edge.
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await handle.pushLiveFrame(altFrame(4));
    await expect.poll(() => liveContent(page).textContent()).toContain("line 6");
  });

  // A full-screen MOUSE app otherwise owns every touch, so the drag that
  // adjusts a selection's handles never reaches WebKit: the callout comes up
  // and its handles will not move. Forwarding yields while a selection is live;
  // the layout does not, so the row keys are untouched and the hold above holds.
  test("a live selection releases the full-screen app's grip on touch gestures", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });
    await handle.pushLiveFrame({ ...altFrame(1), mouse: true, mouseSgr: true });
    await expect.poll(() => liveContent(page).textContent()).toContain("line 1");

    const touchMoveCancelled = () =>
      scroller(page).evaluate((el) => !el.dispatchEvent(new Event("touchmove", { cancelable: true })));
    const touchAction = () => scroller(page).evaluate((el) => getComputedStyle(el).touchAction);

    await expect.poll(touchAction).toBe("none");
    await expect.poll(touchMoveCancelled).toBe(true);

    await selectRow(page, "line 2");

    await expect.poll(touchAction).not.toBe("none");
    await expect.poll(touchMoveCancelled).toBe(false);

    // Dropping it hands the app its gestures back.
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await expect.poll(touchAction).toBe("none");
    await expect.poll(touchMoveCancelled).toBe(true);
  });
});

// #3918: a URL an agent prints renders as a target=_blank anchor. Under a
// full-screen mouse app the scroller preventDefaults the press and takes
// pointer capture; capture retargets pointerup, and so click, to the scroller,
// so the anchor never navigated. Needs real input: jsdom implements neither
// pointer capture nor the retargeting, and dispatchEvent skips the gesture.
const LINK = "https://example.com/pull/1375";
const PROMPT = "$ open the PR";

const mouseBytes = (h: MockHandle) => liveTexts(h).filter((s) => /\x1b\[</.test(s));

/** Centre of `selector`, once the point actually hits it. The mobile sidebar is
 *  an overlay with a 300ms slide-out and `toBeVisible` does not hit-test, so
 *  without this a tap lands on the closing sidebar. */
async function hittableCentre(page: Page, selector: string) {
  const box = (await page.locator(selector).first().boundingBox())!;
  const point = [box.x + box.width / 2, box.y + box.height / 2] as const;
  await expect
    .poll(
      () =>
        page.evaluate(([x, y, sel]) => !!document.elementFromPoint(x as number, y as number)?.closest(sel as string), [
          point[0],
          point[1],
          selector,
        ] as const),
      { timeout: 5_000 },
    )
    .toBe(true);
  return point;
}

async function setupLink(page: Page, mobile: boolean) {
  // Route on the CONTEXT, not the page: the link opens a new tab, and a
  // page-scoped route would leave that tab to hit the real network.
  await page
    .context()
    .route("https://example.com/**", (route) =>
      route.fulfill({ contentType: "text/html", body: "<title>linked page</title>ok" }),
    );
  const handle = await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 14, desktopFontSize: 14 });
  await page.reload();
  if (mobile) await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
  await handle.waitForLiveReady();
  await handle.pushLiveFrame({
    content: [PROMPT, `see ${LINK} for details`, ...Array<string>(22).fill("")].join("\n") + "\n",
    rows: 24,
    history: 0,
    altScreen: true,
    mouse: true,
    mouseSgr: true,
  });
  await expect(scroller(page)).toHaveClass(/overflow-hidden/);
  await expect(page.locator(`a[href="${LINK}"]`).first()).toBeVisible();
  return handle;
}

test.describe("Live terminal link clicks (desktop)", () => {
  test.use(DESKTOP);

  test("a real click on a printed URL opens it in a new tab", async ({ page }) => {
    const handle = await setupLink(page, false);
    const [x, y] = await hittableCentre(page, `a[href="${LINK}"]`);
    // Real mouse input, not dispatchEvent: only the browser's own
    // press/capture/release sequence reproduces the retargeting.
    const [popup] = await Promise.all([page.context().waitForEvent("page"), page.mouse.click(x, y)]);
    await popup.waitForLoadState();
    expect(popup.url()).toBe(LINK);
    // The press that opened the link is the browser's, not the app's.
    expect(mouseBytes(handle)).toHaveLength(0);
  });
});

test.describe("Live terminal link taps (mobile)", () => {
  test.use(iPhone13);

  test("a real tap on a printed URL opens it in a new tab", async ({ page }) => {
    // Touch never enters the forwarding path (it is gated to pointerType
    // "mouse"), but the same anchor has to work under a finger.
    await setupLink(page, true);
    const [x, y] = await hittableCentre(page, `a[href="${LINK}"]`);
    const [popup] = await Promise.all([page.context().waitForEvent("page"), page.touchscreen.tap(x, y)]);
    await popup.waitForLoadState();
    expect(popup.url()).toBe(LINK);
  });

  test("a tap on output beside the link opens nothing", async ({ page }) => {
    await setupLink(page, true);
    const [x, y] = await hittableCentre(page, "[data-live-terminal]");
    const opened: string[] = [];
    page.context().on("page", (popup) => opened.push(popup.url()));
    await page.touchscreen.tap(x, y);
    await observeFor(page, 500, async () => {
      expect(opened).toEqual([]);
      expect(page.context().pages()).toHaveLength(1);
    });
  });
});

// #3342: rows rendered as one `white-space: pre` run take their column
// positions from whatever font supplied each glyph, so a glyph missing from the
// configured font falls back to a non-1-cell advance and shifts every later
// column. The renderer must enforce per-cell width.
test.describe("Live terminal glyph cell-width enforcement", () => {
  test.use(DESKTOP);

  const ASCII_ROW = "AAAAAAAAAAAAAAAAAAAA"; // exactly 20 cells
  // 5x2 CJK + 4 + 2 braille + 4 = 20 cells
  const MIXED_ROW = "한글테스트" + "AAAA" + "⠋⠍" + "AAAA";

  test("a row with glyphs missing from the terminal font is exactly cells x cellWidth wide", async ({ page }) => {
    const handle = await openLiveTerminal(page, { settings: null });

    // Two lines with the SAME terminal cell count (20): one pure ASCII, one
    // mixing CJK and braille that no default stack covers.
    await handle.pushLiveFrame({ content: [ASCII_ROW, MIXED_ROW, ""].join("\n"), rows: 6, history: 0, cursor: null });
    await expect.poll(() => liveContent(page).first().innerText()).toContain("테스트");

    const metrics = await page.evaluate(() => {
      const grid = document.querySelector<HTMLElement>("[data-live-content]")!;
      // Mirror the component's own cell measure: a 20-char M run in the same
      // styles, inside the grid so it inherits font family and size.
      const probe = document.createElement("span");
      probe.textContent = "M".repeat(20);
      probe.setAttribute("aria-hidden", "true");
      probe.style.whiteSpace = "pre";
      probe.style.position = "absolute";
      probe.style.visibility = "hidden";
      grid.appendChild(probe);
      const cellW = probe.getBoundingClientRect().width / 20;
      probe.remove();
      // Row divs are full-width blocks; their INLINE content extent is the
      // laid-out text width, which is what the grid invariant constrains.
      const inlineWidth = (el: HTMLElement) => {
        const range = document.createRange();
        range.selectNodeContents(el);
        return range.getBoundingClientRect().width;
      };
      const rows = [...grid.children]
        .filter((el): el is HTMLElement => el instanceof HTMLElement && el.tagName === "DIV")
        .map((el) => ({ text: el.textContent ?? "", width: inlineWidth(el) }));
      return { cellW, rows };
    });

    const ascii = metrics.rows.find((r) => r.text.startsWith(ASCII_ROW.slice(0, 10)));
    const mixed = metrics.rows.find((r) => r.text.includes("테스트"));
    expect(ascii).toBeDefined();
    expect(mixed).toBeDefined();
    expect(metrics.cellW).toBeGreaterThan(0);
    const expected = 20 * metrics.cellW;
    // The pure-ASCII line already honors the grid; it anchors the measurement.
    expect(Math.abs(ascii!.width - expected)).toBeLessThan(0.75);
    expect(Math.abs(mixed!.width - expected)).toBeLessThan(0.75);
  });
});
