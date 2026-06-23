import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// A fullscreen agent (Claude) only fills part of a tall mobile pane and leaves
// trailing blank rows; it may also park its hardware cursor low in that blank
// region while drawing its own caret in the input box higher up. The overlay
// must not be painted on that blank row at the bottom of the pane (the
// reported "cursor stuck at the bottom"). #2115 follow-up.
//
// The keyboard-open scroll anchoring that pins a cursor-less agent's footer
// above the keyboard needs a shrunken container + visualViewport, which the
// mocked harness can't drive; that path is covered by the live diagnostic.
test.use({ ...devices["iPhone 13"] });

const ROWS = 58;

/** A pane where the agent UI occupies the first `contentRows` rows and the
 *  rest are blank, mirroring a fresh Claude session on a tall phone. */
function fullscreenAgentFrame(contentRows: number, cursor: { x: number; y: number } | null) {
  const lines: string[] = [];
  for (let i = 0; i < contentRows - 1; i++) lines.push(`agent line ${i}`);
  lines.push("FOOTER for shortcuts");
  for (let i = contentRows; i < ROWS; i++) lines.push("");
  return { content: lines.join("\n") + "\n", rows: ROWS, history: 0, cursor };
}

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  await page.waitForTimeout(300);
}

test.describe("Mobile fullscreen-agent layout", () => {
  test("short agent UI bottom-aligns with the trailing blank rows trimmed", async ({ page }) => {
    // Fresh fullscreen agent: no scrollback, so the live-edge buffer has no
    // history to load above the screen (keeps the bottom-align/cursor math on
    // the screen rows alone).
    const handle = await mockTerminalApis(page, { liveHistory: 0 });
    await page.goto("/");
    await openSession(page, handle);

    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 20 }));
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("FOOTER");

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
    // Fresh fullscreen agent: no scrollback, so the live-edge buffer has no
    // history to load above the screen (keeps the bottom-align/cursor math on
    // the screen rows alone).
    const handle = await mockTerminalApis(page, { liveHistory: 0 });
    await page.goto("/");
    await openSession(page, handle);

    // Cursor at row 55, but the agent only drew 22 rows of content; the
    // overlay must be suppressed, not pinned to the blank pane bottom.
    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 55 }));
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("FOOTER");
    await page.waitForTimeout(200);
    await expect(page.locator("[data-live-cursor]")).toHaveCount(0);

    // A cursor INSIDE the content still renders, up in the content (not at the
    // bottom): its top sits above the footer row. (Exact-row alignment is
    // covered against real tmux in live/live-size-owner-takeover.spec.ts.)
    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 10 }));
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
