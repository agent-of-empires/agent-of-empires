// Mobile single-pane layout: the right-panel picker that promotes a view into
// the full viewport, plugin panes, the live renderer, fullscreen-agent
// alignment, the settings header, and the desktop-only toolbar gate.

import { test, expect } from "./helpers/mockedTest";
import { openPanelsByDefault } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { iPhone13 } from "./helpers/viewports";
import { liveContent, openLiveSession, openLiveTerminal } from "./helpers/liveTerminal";

const liveTerminal = (page: Page) => page.locator("[data-live-terminal]").first();
const picker = (page: Page) => page.getByTestId("mobile-right-panel-picker");
const tab = (page: Page, view: string) => page.getByTestId(`mobile-tab-${view}`);

async function openMore(page: Page) {
  await tab(page, "more").click();
  await expect(picker(page)).toBeVisible({ timeout: 5_000 });
}

/** One-finger horizontal swipe dispatched on the window, where useDrawerSwipe listens. */
async function swipe(page: Page, fromX: number, toX: number, y: number) {
  await page.evaluate(
    ({ fromX, toX, y }) => {
      const fire = (type: string, x: number) => {
        const touch = new Touch({ identifier: 0, target: document.body, clientX: x, clientY: y });
        const lifted = type === "touchend";
        document.body.dispatchEvent(
          new TouchEvent(type, {
            bubbles: true,
            touches: lifted ? [] : [touch],
            changedTouches: [touch],
          }),
        );
      };
      fire("touchstart", fromX);
      for (let i = 1; i <= 5; i++) fire("touchmove", fromX + ((toX - fromX) * i) / 5);
      fire("touchend", toX);
    },
    { fromX, toX, y },
  );
}

/** Shrink `visualViewport` the way an on-screen keyboard does. */
async function simulateKeyboardOpen(page: Page, keyboardPx: number) {
  await page.evaluate((keyboardPx) => {
    const vv = window.visualViewport;
    if (!vv) return;
    const height = window.innerHeight - keyboardPx;
    Object.defineProperty(vv, "height", { get: () => height, configurable: true });
    Object.defineProperty(vv, "offsetTop", { get: () => 0, configurable: true });
    vv.dispatchEvent(new Event("resize"));
  }, keyboardPx);
}

/** The picked pane must reserve the home-indicator inset the App root no longer
 *  does (moved per-surface; see index.css .safe-area-inset). */
async function expectSafeAreaInset(page: Page, testId: string) {
  const layer = page.getByTestId(testId);
  // Terminal layers clear it through a class the mobile key row can turn off.
  if (await layer.evaluate((el) => el.classList.contains("home-indicator-clearance"))) return;
  const inset = await layer.evaluate((el) => (el as HTMLElement).style.paddingBottom);
  expect(inset).toContain("safe-area-inset-bottom");
}

// #1452: on mobile a tab bar promotes the Session's views into the single full-viewport pane, so the paired
// terminal stays tall under the keyboard; More holds the rest.
test.describe("Mobile session view tabs (#1452)", () => {
  test.use(iPhone13);

  test("a tab bar switches the Session's views, and More rises as a bottom sheet with the rest", async ({ page }) => {
    await openLiveTerminal(page, { mobile: true, settings: null });
    const viewport = page.viewportSize()!;
    await expect(tab(page, "agent")).toHaveAttribute("aria-current", "page");
    // The header holds no panels toggle: the tabs replace it.
    await expect(page.getByRole("button", { name: "Toggle panels" })).toHaveCount(0);

    await openMore(page);
    // A full-width sheet resting on the bottom edge, not a side drawer.
    await expect
      .poll(async () => {
        const box = await picker(page).boundingBox();
        return box && { left: box.x, width: box.width, bottom: Math.round(box.y + box.height) };
      })
      .toEqual({ left: 0, width: viewport.width, bottom: viewport.height });
    // Tailwind v4 `translate-y-*` sets `translate`, so a transition on `transform` alone would snap it.
    expect(await picker(page).evaluate((el) => getComputedStyle(el).transitionProperty)).toContain("translate");
    // The tabs' own views stay out of More.
    await expect(page.getByTestId("mobile-right-panel-pick-agent")).toHaveCount(0);
    await page.getByTestId("mobile-right-panel-picker-backdrop").click({ position: { x: 20, y: 80 } });
    await expect(picker(page)).toBeHidden();

    // A swipe left raises nothing; a swipe right goes back to the session list.
    const y = viewport.height / 2;
    await swipe(page, viewport.width / 2 + 80, viewport.width / 2 - 80, y);
    await expect(picker(page)).toBeHidden();
    await expect(page.getByTestId("topbar-back")).toBeVisible();
    await swipe(page, viewport.width / 2 - 80, viewport.width / 2 + 80, y);
    await expect(page.getByTestId("sidebar-new-session-pill")).toBeInViewport();
    await expect(page.getByTestId("topbar-back")).toHaveCount(0);
  });

  test("adjusting selected chat text neither raises More nor goes back", async ({ page }) => {
    await openLiveTerminal(page, { mobile: true, settings: null });
    const content = liveContent(page).first();
    await expect(content).toContainText("$ ready");
    const selected = await content.evaluate((el) => {
      const range = document.createRange();
      range.selectNodeContents(el);
      const selection = document.getSelection()!;
      selection.removeAllRanges();
      selection.addRange(range);
      return selection.toString();
    });
    expect(selected).toContain("$ ready");
    const { width, height } = page.viewportSize()!;
    for (const [from, to] of [
      [width / 2 + 80, width / 2 - 80],
      [width / 2 - 80, width / 2 + 80],
    ]) {
      await swipe(page, from, to, height / 2);
      await expect(picker(page)).toBeHidden();
      await expect(page.getByTestId("topbar-back")).toBeVisible();
      expect(await page.evaluate(() => document.getSelection()?.toString())).toBe(selected);
    }
    await page.evaluate(() => document.getSelection()?.removeAllRanges());
    await swipe(page, width / 2 - 80, width / 2 + 80, height / 2);
    await expect(page.getByTestId("topbar-back")).toHaveCount(0);
  });

  test("the Terminal tab promotes the paired terminal and it survives the keyboard", async ({ page }) => {
    await openLiveTerminal(page, { mobile: true, settings: { mobileFontSize: 10 } });
    await tab(page, "paired").click();
    const paired = page.locator('[data-term="paired"]');
    await paired.waitFor({ state: "visible", timeout: 10_000 });
    await expectSafeAreaInset(page, "mobile-paired-layer");

    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => (await paired.boundingBox())?.height ?? 0, {
        message: "paired terminal collapsed under the keyboard",
      })
      .toBeGreaterThan(150);

    // Back to the agent: the paired shell stays mounted, keeping its PTY and scrollback.
    await tab(page, "agent").click();
    await expect(paired).toHaveCount(1);
    await expect(liveTerminal(page)).toBeVisible();
  });

  test("promotes the paired shell over a structured view session and survives the keyboard", async ({ page }) => {
    const handle = await mockTerminalApis(page, {
      sessionFields: { title: "acp-mobile", view: "structured", acp_worker_state: "running" },
    });
    await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
    await openLiveSession(page, handle, { mobile: true, settings: null, title: "acp-mobile", waitForLive: false });
    await tab(page, "paired").waitFor({ state: "visible", timeout: 10_000 });
    await tab(page, "paired").click();
    const paired = page.locator('[data-term="paired"]');
    await paired.waitFor({ state: "visible", timeout: 10_000 });

    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => (await paired.boundingBox())?.height ?? 0, {
        message: "paired terminal collapsed on a structured view session",
      })
      .toBeGreaterThan(150);

    await tab(page, "agent").click();
    await expect(tab(page, "agent")).toHaveAttribute("aria-current", "page");
  });
});

test.describe("Desktop right panel split is unchanged (#1452)", () => {
  test.use({ viewport: { width: 1400, height: 900 }, hasTouch: false });

  // The mobile touch toolbar is gated on (pointer: coarse) in useMobileKeyboard,
  // which never matches a desktop project even with a session open.
  test("renders the side-by-side split and no mobile picker or touch toolbar", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openPanelsByDefault(page);
    await openLiveSession(page, handle, { settings: null });
    await expect(page).toHaveURL(/\/session\/pinch-test/, { timeout: 10_000 });

    await expect(page.getByTestId("content-split-resize-handle")).toBeVisible();
    await expect(page.getByTestId("panel-control")).toBeVisible();
    await expect(page.getByTestId("mobile-view-tabs")).toHaveCount(0);
    await expect(picker(page)).toBeHidden();
    await expect(page.getByRole("button", { name: "Compose", exact: true })).toHaveCount(0);
    await expect(page.getByRole("group", { name: "Arrow keys joystick" })).toHaveCount(0);
  });
});

test.describe("Mobile agent pane", () => {
  test.use(iPhone13);

  // On touch-primary devices the agent pane renders the capture-snapshot live
  // view: real DOM text, native scrolling, no xterm.js and no canvases. This
  // sidesteps the WebKit WebGL corruption (xtermjs/xterm.js#5816) and gives iOS
  // native text selection.
  test("no touch toolbar without a session; the agent pane renders the live view, not xterm", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("button", { name: /Arrow/ })).toHaveCount(0);

    await openLiveSession(page, handle, { mobile: true, settings: null });
    await expect(liveContent(page)).toBeAttached();
    await expect(page.locator("[data-live-terminal] canvas")).toHaveCount(0);
    await expect(page.locator(".xterm")).toHaveCount(0);
    await expect.poll(() => liveContent(page).first().innerText()).toContain("$ ready");
  });
});

// A fullscreen agent (Claude) fills only part of a tall mobile pane and leaves
// trailing blank rows; it may also park its hardware cursor low in that blank
// region while drawing its own caret higher up. The overlay must not be painted
// on that blank bottom row. #2115 follow-up.
//
// The keyboard-open scroll anchoring that pins a cursor-less agent's footer
// above the keyboard needs a shrunken container plus visualViewport, which the
// mocked harness cannot drive; the live diagnostic covers that path.
test.describe("Mobile fullscreen-agent layout", () => {
  test.use(iPhone13);

  const ROWS = 58;

  /** A pane where the agent UI occupies the first `contentRows` rows and the
   *  rest are blank, mirroring a fresh Claude session on a tall phone. */
  function frame(contentRows: number, cursor: { x: number; y: number } | null) {
    const lines = Array.from({ length: contentRows - 1 }, (_, i) => `agent line ${i}`);
    lines.push("FOOTER for shortcuts");
    for (let i = contentRows; i < ROWS; i++) lines.push("");
    return { content: lines.join("\n") + "\n", rows: ROWS, history: 0, cursor };
  }

  /** Fresh fullscreen agent: no scrollback, so the live-edge buffer has no
   *  history above the screen and the alignment math stays on the screen rows. */
  async function openFreshAgent(page: Page): Promise<MockHandle> {
    return openLiveSession(page, await mockTerminalApis(page, { liveHistory: 0 }), { mobile: true, settings: null });
  }

  test("short agent UI bottom-aligns with the trailing blank rows trimmed", async ({ page }) => {
    const handle = await openFreshAgent(page);
    await handle.pushLiveFrame(frame(22, { x: 2, y: 20 }));
    await expect.poll(() => liveContent(page).first().innerText()).toContain("FOOTER");

    const r = await page.evaluate(() => {
      const content = document.querySelector("[data-live-content]")!;
      const rows = Array.from(content.children).filter((el) => !el.hasAttribute("data-live-cursor"));
      const footer = rows.find((el) => (el.textContent ?? "").includes("FOOTER"))!;
      const scroller = document.querySelector("[data-live-terminal] > div")!;
      return {
        renderedRows: rows.length,
        footerToBottom: scroller.getBoundingClientRect().bottom - footer.getBoundingClientRect().bottom,
      };
    });
    // 22 rows rendered, not all 58: trailing blanks trimmed.
    expect(r.renderedRows).toBeLessThan(30);
    // Footer hugs the scroller bottom (a spare line or two), not a dozen-row gap.
    expect(r.footerToBottom).toBeLessThan(40);
  });

  test("cursor parked below the captured content is not painted at the bottom", async ({ page }) => {
    const handle = await openFreshAgent(page);

    // Cursor at row 55 while the agent drew only 22 rows: the overlay must be
    // suppressed, not pinned to the blank pane bottom.
    await handle.pushLiveFrame(frame(22, { x: 2, y: 55 }));
    await expect.poll(() => liveContent(page).first().innerText()).toContain("FOOTER");
    await expect(page.locator("[data-live-cursor]")).toHaveCount(0);

    // A cursor INSIDE the content still renders, above the footer row. (Exact
    // row alignment is covered against real tmux in
    // live/live-size-owner-takeover.spec.ts.)
    await handle.pushLiveFrame(frame(22, { x: 2, y: 10 }));
    await expect(page.locator("[data-live-cursor]")).toHaveCount(1);
    const aboveFooter = await page.evaluate(() => {
      const rows = Array.from(document.querySelectorAll("[data-live-content] > div")).filter(
        (el) => !el.hasAttribute("data-live-cursor"),
      );
      const footer = rows.find((el) => (el.textContent ?? "").includes("FOOTER"))!;
      const cursor = document.querySelector("[data-live-cursor]")!;
      return cursor.getBoundingClientRect().top < footer.getBoundingClientRect().top;
    });
    expect(aboveFooter).toBe(true);
  });
});

// #1430: Back and the title share a row, and nothing pushes the header past the
// viewport. Settings opens on a grouped section list; a section's Back returns to it.
test.describe("Mobile settings", () => {
  test.use(iPhone13);

  test("opens on a grouped list, pushes a section, and Back returns to the list", async ({ page }) => {
    await page.goto("/settings");
    const list = page.getByTestId("settings-section-list");
    await expect(list.getByText("Dashboard", { exact: true })).toBeVisible();
    const backBtn = page.getByRole("button", { name: /Back/ });
    const backBox = (await backBtn.boundingBox())!;
    const titleBox = (await page.getByText("Settings", { exact: true }).first().boundingBox())!;
    expect(Math.abs(titleBox.y - backBox.y)).toBeLessThan(backBox.height);
    expect(titleBox.x - (backBox.x + backBox.width)).toBeGreaterThan(6);

    await list.getByRole("button", { name: /Notifications/ }).click();
    await expect(page).toHaveURL(/\/settings\/notifications$/);
    await expect(page.getByRole("heading", { name: "Notifications", level: 2 })).toBeVisible();
    await expect(list).toHaveCount(0);
    await page.getByRole("button", { name: /All settings/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expect(list).toBeVisible();

    const header = page.getByTestId("settings-header");
    for (const width of [390, 320]) {
      await page.setViewportSize({ width, height: 640 });
      await expect
        .poll(() =>
          header.evaluate((el) => el.scrollWidth <= el.clientWidth && el.getBoundingClientRect().width <= innerWidth),
        )
        .toBe(true);
    }

    // Returning to the list pops the tab rather than stacking another entry.
    await page.goBack();
    await expect(page).not.toHaveURL(/\/settings/);
    await page.goto("/settings/sandbox");
    await page.getByRole("button", { name: /All settings/ }).click();
    await expect(list).toBeVisible();
    await page.goBack();
    await expect(page).not.toHaveURL(/\/settings\/sandbox$/);
  });
});
