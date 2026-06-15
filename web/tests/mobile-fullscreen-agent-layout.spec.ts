import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";

// A fullscreen agent (Claude) only fills part of a tall mobile pane: it draws
// its UI near the top and leaves many trailing blank rows. Two bugs lived here
// (#2115 follow-ups): (1) the blank rows rendered as dead space, floating the
// input box far above the keyboard; (2) when the agent parked its hardware
// cursor low in that blank region, the overlay's row index overflowed and the
// cursor pinned to the very bottom of the pane. This pins both behaviors.
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
  test("short agent UI bottom-aligns near the keyboard with no dead gap", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 20 }));
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("FOOTER");

    const gap = await page.evaluate(() => {
      const content = document.querySelector("[data-live-content]")!;
      const rows = Array.from(content.children).filter((el) => !el.hasAttribute("data-live-cursor"));
      const footer = rows.find((el) => (el.textContent ?? "").includes("FOOTER"))!;
      const scroller = document.querySelector("[data-live-terminal] > div")!;
      // Trailing blank rows must not be rendered, and the footer must sit at
      // the bottom of the scroller (small gap = 1-2 spare lines, not dozens).
      return {
        renderedRows: rows.length,
        footerToBottom: scroller.getBoundingClientRect().bottom - footer.getBoundingClientRect().bottom,
      };
    });
    // 22 content rows rendered, not all 58: trailing blanks trimmed.
    expect(gap.renderedRows).toBeLessThan(30);
    // Footer hugs the scroller bottom (allow a couple spare lines), not a
    // dozen-row gap.
    expect(gap.footerToBottom).toBeLessThan(60);
  });

  test("cursor parked below the captured content is not painted at the bottom", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page, handle);

    // Cursor at row 55, but the agent only drew 22 rows of content (some tmux
    // builds trim the blanks); the overlay must be suppressed, not pinned to
    // the pane bottom.
    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 55 }));
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("FOOTER");
    await page.waitForTimeout(200);
    await expect(page.locator("[data-live-cursor]")).toHaveCount(0);

    // A cursor INSIDE the content still renders.
    handle.pushLiveFrame(fullscreenAgentFrame(22, { x: 2, y: 10 }));
    await expect(page.locator("[data-live-cursor]")).toHaveCount(1);
  });
});
