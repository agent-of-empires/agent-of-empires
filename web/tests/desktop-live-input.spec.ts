import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

// #2115: clicking the non-focusable rendered pane must refocus the hidden input so typing reaches the pane.
test.describe("Desktop live terminal input", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("a click focuses the input and marks the pane; keys reach the PTY at the desktop font size", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const pane = page.locator('[data-term="agent"]').first();
    await pane.waitFor({ state: "visible", timeout: 10_000 });
    // A fine pointer uses desktopFontSize (14px), not mobileFontSize.
    const content = page.locator("[data-live-content]").first();
    await expect
      .poll(() => content.evaluate((el) => getComputedStyle(el.closest("[data-live-terminal] > div")!).fontSize))
      .toBe("14px");

    await page.locator("[data-live-terminal]").first().click();
    const input = page.locator('textarea[aria-label="Live terminal input"]').first();
    await expect(input).toBeFocused();
    // The focused pane gets the terminal-active ring, like the TUI's active border.
    await expect(pane).toHaveAttribute("data-pane-focused", "true");

    const sent = (from: number) => handle.liveMessages.slice(from).map((m) => m.toString("utf8"));
    let before = handle.liveInput.length;
    await page.keyboard.type("ls");
    await expect.poll(() => Buffer.concat(handle.liveInput.slice(before)).toString()).toBe("ls");

    // Shift+Tab is backtab (CSI Z), which Claude Code's mode cycle reads, not a plain Tab.
    before = handle.liveMessages.length;
    await page.keyboard.press("Shift+Tab");
    await expect.poll(() => sent(before)).toContainEqual("\x1b[Z");
    expect(sent(before)).not.toContainEqual("\t");

    // Codex's Alt+V image paste: ESC + v, like a native terminal.
    before = handle.liveMessages.length;
    await page.keyboard.press("Alt+KeyV");
    await expect.poll(() => sent(before)).toContainEqual("\x1bv");

    await input.blur();
    await expect(pane).not.toHaveAttribute("data-pane-focused", "true");
  });

  test("Ctrl+Shift+C copies the selection without ^C, and Ctrl+V pastes as a bracketed paste without ^V", async ({
    page,
  }) => {
    // #2384: Ctrl+Shift+C copies the DOM selection; the focused hidden input would copy nothing, and Ctrl+C stays SIGINT.
    const handle = await mockTerminalApis(page);
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-content]").first().waitFor({ state: "visible", timeout: 10_000 });

    // A second, wider frame arrives after mount; select only once content stops changing.
    let prevContent = "";
    await expect
      .poll(
        async () => {
          const cur = await page.evaluate(() => document.querySelector("[data-live-content]")?.textContent ?? "");
          const stable = cur !== "" && cur === prevContent;
          prevContent = cur;
          return stable;
        },
        { timeout: 10_000, intervals: [200] },
      )
      .toBe(true);

    await page.locator("[data-live-terminal]").first().click();
    const selected = await page.evaluate(() => {
      const content = document.querySelector("[data-live-content]")!;
      const row = Array.from(content.querySelectorAll("div")).find((d) => (d.textContent ?? "").trim().length > 0);
      if (!row) throw new Error("no non-empty terminal row to select");
      const range = document.createRange();
      range.selectNodeContents(row);
      const sel = window.getSelection()!;
      sel.removeAllRanges();
      sel.addRange(range);
      return sel.toString();
    });
    expect(selected.trim().length).toBeGreaterThan(0);
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();

    const before = handle.liveMessages.length;
    await page.keyboard.press("Control+Shift+C");

    await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(selected);
    const sentSigint = handle.liveMessages.slice(before).some((m) => m.toString("utf8") === "\x03");
    expect(sentSigint).toBe(false);

    // #2384: Ctrl+V falls through to the native paste event instead of sending ^V.
    await page.evaluate(() => navigator.clipboard.writeText("pasted text"));
    await page.evaluate(() => window.getSelection()?.removeAllRanges());
    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();
    const beforePaste = handle.liveMessages.length;
    await page.keyboard.press("Control+v");
    await expect
      .poll(() => handle.liveMessages.slice(beforePaste).map((m) => m.toString("utf8")))
      .toContainEqual("\x1b[200~pasted text\x1b[201~");
    expect(handle.liveMessages.slice(beforePaste).some((m) => m.toString("utf8") === "\x16")).toBe(false);
  });

  test("an agent OSC 52 copy reaches the browser clipboard after mouse release", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const scroller = page.locator("[data-live-terminal] > div").first();
    await scroller.waitFor({ state: "visible", timeout: 10_000 });
    await handle.pushLiveFrame({
      content: "OpenCode selection\n",
      rows: 24,
      history: 0,
      cursor: null,
      altScreen: true,
      mouse: true,
      mouseSgr: true,
    });
    await expect.poll(() => scroller.getAttribute("class")).toContain("overflow-hidden");

    const box = await scroller.boundingBox();
    if (!box) throw new Error("terminal has no bounding box");
    await page.mouse.move(box.x + 20, box.y + 20);
    await page.mouse.down();
    await page.mouse.move(box.x + 100, box.y + 20);
    await page.mouse.up();
    handle.pushLiveClipboard("copied through live-ws");

    await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe("copied through live-ws");
  });

  test("scrolling down to the bottom keeps real rows visible", async ({ page }) => {
    const handle = await mockTerminalApis(page, { liveHistory: 600, delayLiveWindowShrinkMs: 80 });
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await handle.waitForLiveReady();
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");

    const scroller = page.locator("[data-live-terminal] > div").first();
    await scroller.evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.45;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect.poll(() => scroller.evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    await expect(page.locator("[data-live-content]")).toContainText("history line");
    await page.evaluate(() => {
      const state = window as typeof window & {
        __BOTTOM_TRANSITION_SAMPLES__?: Array<{
          top: number;
          bottom: number;
          scrollLeft: number;
          visibleText: string;
          firstRowLeft: number | null;
          scrollerLeft: number;
        }>;
        __BOTTOM_TRANSITION_SAMPLING__?: boolean;
      };
      state.__BOTTOM_TRANSITION_SAMPLES__ = [];
      state.__BOTTOM_TRANSITION_SAMPLING__ = true;

      const sample = () => {
        const el = document.querySelector<HTMLElement>("[data-live-terminal] > div");
        if (el) {
          const scrollerRect = el.getBoundingClientRect();
          const visibleRows = Array.from(el.querySelectorAll<HTMLElement>("[data-live-content] > div"))
            .filter((row) => !row.hasAttribute("aria-hidden"))
            .filter((row) => {
              const rect = row.getBoundingClientRect();
              return (
                rect.bottom > scrollerRect.top &&
                rect.top < scrollerRect.bottom &&
                (row.textContent ?? "").trim() !== ""
              );
            });
          const firstRect = visibleRows[0]?.getBoundingClientRect();
          state.__BOTTOM_TRANSITION_SAMPLES__!.push({
            top: el.scrollTop,
            bottom: el.scrollHeight - el.clientHeight,
            scrollLeft: el.scrollLeft,
            visibleText: visibleRows.map((row) => row.textContent ?? "").join("|"),
            firstRowLeft: firstRect?.left ?? null,
            scrollerLeft: scrollerRect.left,
          });
        }
        if (state.__BOTTOM_TRANSITION_SAMPLING__) requestAnimationFrame(sample);
      };
      requestAnimationFrame(sample);
    });
    await scroller.hover();
    for (let i = 0; i < 6; i++) await page.mouse.wheel(0, 5000);
    // Sample until the live edge has rendered, then check every frame on the way.
    await expect
      .poll(() =>
        page.evaluate(() => {
          const samples =
            (window as typeof window & { __BOTTOM_TRANSITION_SAMPLES__?: Array<{ visibleText: string }> })
              .__BOTTOM_TRANSITION_SAMPLES__ ?? [];
          return samples.at(-1)?.visibleText ?? "";
        }),
      )
      .toContain("$ ready");

    const samples = await page.evaluate(() => {
      const state = window as typeof window & {
        __BOTTOM_TRANSITION_SAMPLES__?: Array<{
          top: number;
          bottom: number;
          scrollLeft: number;
          visibleText: string;
          firstRowLeft: number | null;
          scrollerLeft: number;
        }>;
        __BOTTOM_TRANSITION_SAMPLING__?: boolean;
      };
      state.__BOTTOM_TRANSITION_SAMPLING__ = false;
      return state.__BOTTOM_TRANSITION_SAMPLES__ ?? [];
    });

    const reachedBottom = samples.findIndex((sample) => sample.bottom - sample.top < 2);
    expect(reachedBottom, "wheel scrolling reaches the live edge").toBeGreaterThanOrEqual(0);
    const blankFrame = samples.slice(reachedBottom).find((sample) => sample.visibleText === "");
    expect(blankFrame, "every bottom-transition frame shows rendered terminal rows").toBeUndefined();

    const final = samples.at(-1)!;
    expect(final.scrollLeft).toBe(0);
    expect(final.visibleText).toContain("$ ready");
    expect(final.firstRowLeft, "visible rows start at the terminal's left edge").not.toBeNull();
    expect(Math.abs(final.firstRowLeft! - final.scrollerLeft)).toBeLessThan(2);

    await expect
      .poll(() =>
        textMessages(handle)
          .filter((m) => m.includes('"type":"window"'))
          .at(-1),
      )
      .toContain('"lines":');
  });
});

function textMessages(handle: Awaited<ReturnType<typeof mockTerminalApis>>): string[] {
  return handle.liveMessages.map((m) => m.toString("utf8"));
}
