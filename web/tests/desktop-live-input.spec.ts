import { test, expect } from "./helpers/mockedTest";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { clickSidebarSession } from "./helpers/sidebar";

// Regression: on a fine-pointer desktop the unified live view must be
// interactive, not view-only. The rendered pane is plain (non-focusable) DOM
// text, so clicking it blurred the hidden input to <body> and keystrokes went
// nowhere; the session looked read-only. A plain click must (re)focus the
// input so typing reaches the pane. (#2115 follow-up to the xterm removal.)
test.describe("Desktop live terminal input", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("clicking the terminal focuses the input and keystrokes are sent", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });

    // Click into the terminal body (the instinctive "I want to type here"),
    // which previously blurred focus to <body>.
    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();

    // Typing now produces input bytes on the live WS (binary frames).
    const before = handle.liveMessages.filter((m) => m instanceof Buffer && m.length > 0).length;
    await page.keyboard.type("ls");
    await expect
      .poll(() => handle.liveMessages.filter((m) => m instanceof Buffer && m.length > 0).length)
      .toBeGreaterThan(before);
  });

  test("the focused pane is marked selected, like the TUI's active border", async ({ page }) => {
    // On a multi-pane desktop it must be obvious which box keystrokes go to.
    // LiveTerminalView frames the focused pane with the teal `terminal-active`
    // ring and flags it `data-pane-focused`; blurring drops the marker.
    await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const pane = page.locator('[data-term="agent"]').first();
    await pane.waitFor({ state: "visible", timeout: 10_000 });

    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();
    await expect(pane).toHaveAttribute("data-pane-focused", "true");

    await page.locator('textarea[aria-label="Live terminal input"]').first().blur();
    await expect(pane).not.toHaveAttribute("data-pane-focused", "true");
  });

  test("Ctrl+V pastes as a bracketed paste instead of sending a literal ^V", async ({ page }) => {
    // The Ctrl+letter chord handler used to swallow Ctrl+V into a ^V (0x16) to
    // tmux and preventDefault the keydown, blocking the browser's paste event.
    // It must now fall through so the native paste reaches onPaste. (#2384)
    const handle = await mockTerminalApis(page);
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();

    await page.evaluate(() => navigator.clipboard.writeText("pasted text"));
    const before = handle.liveMessages.length;
    await page.keyboard.press("Control+v");

    await expect
      .poll(() => handle.liveMessages.slice(before).map((m) => m.toString("utf8")))
      .toContainEqual("\x1b[200~pasted text\x1b[201~");
    const sentCtrlV = handle.liveMessages.slice(before).some((m) => m.toString("utf8") === "\x16");
    expect(sentCtrlV).toBe(false);
  });

  test("Alt+V reaches the terminal as a Meta-v sequence", async ({ page }) => {
    // Codex uses Alt+V as an image-paste shortcut in terminal mode. The live
    // terminal must send the same bytes a native terminal sends: ESC + v.
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();

    const before = handle.liveMessages.length;
    await page.keyboard.press("Alt+KeyV");

    await expect.poll(() => handle.liveMessages.slice(before).map((m) => m.toString("utf8"))).toContainEqual("\x1bv");
  });

  test("Ctrl+Shift+C copies the terminal selection without sending ^C", async ({ page }) => {
    // The hidden input is focused, so the browser's own copy targets the empty
    // textarea; the handler reads the rendered DOM selection and copies it
    // explicitly. Plain Ctrl+C stays SIGINT. (#2384)
    const handle = await mockTerminalApis(page);
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-content]").first().waitFor({ state: "visible", timeout: 10_000 });

    // The component requests a wider scrollback window after mount, so a second
    // frame arrives with history rows. Wait for the content to stop changing
    // before selecting; otherwise the captured Range drifts onto a row that
    // renders in the later frame and the clipboard no longer matches `selected`.
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

    // Focus the input the way a user does (click the pane), then select a
    // rendered terminal row. The selection lives in the DOM while the hidden
    // input keeps focus, which is exactly the state Ctrl+Shift+C must read.
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
  });

  test("Shift+Tab sends backtab (CSI Z), not a plain Tab", async ({ page }) => {
    // The keydown handler keyed only on e.key === "Tab" and always returned
    // "\t", dropping the Shift. Shift+Tab must reach the agent as the backtab
    // sequence \x1b[Z, which is what the TUI's live_send already emits for BTab.
    // Without it, apps that read backtab (e.g. Claude Code's permission-mode
    // cycle) never see Shift+Tab in the web terminal.
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await page.locator("[data-live-terminal]").first().click();
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeFocused();

    const before = handle.liveMessages.length;
    await page.keyboard.press("Shift+Tab");

    await expect.poll(() => handle.liveMessages.slice(before).map((m) => m.toString("utf8"))).toContainEqual("\x1b[Z");
    const sentPlainTab = handle.liveMessages.slice(before).some((m) => m.toString("utf8") === "\t");
    expect(sentPlainTab).toBe(false);
  });

  test("renders at the desktop font size, not the small mobile default", async ({ page }) => {
    // The live view used to always read `mobileFontSize` (default 8px), so on
    // desktop it came up tiny and ignored the dashboard's terminal font-size
    // control. A fine pointer must use `desktopFontSize` (default 14px).
    await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    const content = page.locator("[data-live-content]").first();
    await content.waitFor({ state: "visible", timeout: 10_000 });
    const px = await content.evaluate((el) => getComputedStyle(el.closest("[data-live-terminal] > div")!).fontSize);
    expect(px).toBe("14px");
  });

  test("scrolling down to the bottom keeps real rows visible", async ({ page }) => {
    const handle = await mockTerminalApis(page, { liveHistory: 600, delayLiveWindowShrinkMs: 80 });
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");

    const scroller = page.locator("[data-live-terminal] > div").first();
    await scroller.evaluate((el) => {
      el.scrollTop = el.scrollHeight * 0.45;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect.poll(() => scroller.evaluate((el) => el.scrollHeight), { timeout: 3_000 }).toBeGreaterThan(8000);

    await page.waitForTimeout(300);
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
    for (let i = 0; i < 6; i++) {
      await page.mouse.wheel(0, 5000);
      await page.waitForTimeout(16);
    }
    await page.waitForTimeout(900);

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
