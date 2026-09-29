import { test, expect, observeFor } from "./helpers/mockedTest";
import type { Locator, Page } from "@playwright/test";
import type { MockHandle } from "./helpers/terminal-mocks";
import { iPhone13 } from "./helpers/viewports";
import { liveContent, liveTexts, openLiveTerminal } from "./helpers/liveTerminal";

// A finger tap on a full-screen mouse app (Claude Code's fullscreen mode) is a left click at the tapped cell,
// through the real touch pipeline, so a synthesized click or compatibility mouse event would show up here.
test.describe("Live terminal tap-to-click (mobile)", () => {
  test.use(iPhone13);

  const textarea = (page: Page) => page.locator('textarea[aria-label="Live terminal input"]');
  const sgrReports = (h: MockHandle) => liveTexts(h).filter((s) => s.startsWith("\x1b[<"));

  async function mount(page: Page, lines: string[], mouseSgr = true) {
    const handle = await openLiveTerminal(page, { mobile: true });
    await handle.pushLiveFrame({
      content: `${[...lines, ...Array<string>(24 - lines.length).fill("")].join("\n")}\n`,
      rows: 24,
      history: 0,
      altScreen: true,
      mouse: true,
      mouseSgr,
    });
    return handle;
  }

  /** Taps the first cell of `row` once the point hits the terminal rather than the closing sidebar. */
  async function tapFirstCell(page: Page, row: Locator) {
    const box = (await row.boundingBox())!;
    const [x, y] = [box.x + 3, box.y + box.height / 2];
    await expect
      .poll(() =>
        page.evaluate(([px, py]) => !!document.elementFromPoint(px!, py!)?.closest("[data-live-content]"), [x, y]),
      )
      .toBe(true);
    await page.touchscreen.tap(x, y);
  }

  test("a tap sends one SGR press and release at the tapped row and leaves the keyboard down", async ({ page }) => {
    const handle = await mount(page, ["first", "second", "third"]);
    await tapFirstCell(page, page.getByText("second", { exact: true }));
    await expect.poll(() => sgrReports(handle)).toEqual(["\x1b[<0;1;2M", "\x1b[<0;1;2m"]);
    await observeFor(page, 300, async () => {
      expect(sgrReports(handle)).toHaveLength(2);
      await expect(textarea(page)).not.toBeFocused();
    });
  });

  test("a tap on a legacy-mouse app sends X10 press and release", async ({ page }) => {
    const handle = await mount(page, ["first", "second"], false);
    await tapFirstCell(page, page.getByText("second", { exact: true }));
    const x10 = (btn: number) => String.fromCharCode(0x1b, 0x5b, 0x4d, btn + 32, 1 + 32, 2 + 32);
    await expect.poll(() => liveTexts(handle).filter((s) => s.startsWith("\x1b[M"))).toEqual([x10(0), x10(3)]);
  });

  test("a tap on a wrapped continuation row reports the pane column", async ({ page }) => {
    // Wider than the phone's render width, so the line wraps.
    const long = Array.from({ length: 160 }, (_, i) => String.fromCharCode(97 + (i % 26))).join("");
    const handle = await mount(page, ["top", long]);
    const rows = liveContent(page).locator(":scope > div:not([aria-hidden])");
    await expect.poll(() => rows.count()).toBeGreaterThan(25);
    const firstWrapWidth = (await rows.nth(1).innerText()).length;
    expect(firstWrapWidth).toBeLessThan(long.length);
    await tapFirstCell(page, rows.nth(2));
    await expect.poll(() => sgrReports(handle)[0]).toBe(`\x1b[<0;${firstWrapWidth + 1};2M`);
  });
});
