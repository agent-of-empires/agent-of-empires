import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, installTerminalSpies, seedSettings } from "./helpers/terminal-mocks";

// Select-to-copy over a full-screen agent, driven end to end: a real
// Selection over the production bundle, fed by frames off the live-ws route.
// jsdom covers the hold's logic (MobileLiveTerminal.selectionHold.test.tsx)
// and models the range collapse faithfully, so this is not the cheapest test
// that catches the bug; it is here because the bug is Selection semantics and
// jsdom only simulates those. The mocked suite is Chromium, so it says
// nothing about the WebKit callout this ultimately exists for.
test.use({ ...devices["iPhone 13"] });

// A full-screen agent: no scrollback, and the transcript slides up through
// a fixed grid, so every screen row holds new text on the next frame.
function altFrame(n: number) {
  const lines = [`line ${n}`, `line ${n + 1}`, `line ${n + 2}`, "", "> prompt"];
  return {
    content: lines.join("\n") + "\n",
    rows: 5,
    history: 0,
    cursor: null,
    altScreen: true,
    mouse: false,
    mouseSgr: false,
  };
}

const content = (page: Page) => page.locator("[data-live-content]");
const selection = (page: Page) => page.evaluate(() => window.getSelection()?.toString() ?? "");

test("a selection over a full-screen agent survives its repaints", async ({ page }) => {
  await installTerminalSpies(page);
  const handle = await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 14 });
  await page.reload();
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);

  handle.pushLiveFrame(altFrame(1));
  await expect.poll(() => content(page).textContent()).toContain("line 1");

  // Both endpoints inside the row's text node, the way a long-press word
  // selection anchors. Anchoring on the row element instead would put them
  // outside the rewritten data and survive a repaint a real gesture cannot.
  await page.evaluate(() => {
    const row = Array.from(document.querySelectorAll("[data-live-content] > div")).find(
      (r) => r.textContent === "line 2",
    );
    if (!row) throw new Error("row not rendered");
    const node = document.createTreeWalker(row, NodeFilter.SHOW_TEXT).nextNode() as Text;
    const range = document.createRange();
    range.setStart(node, 0);
    range.setEnd(node, node.data.length);
    const sel = window.getSelection()!;
    sel.removeAllRanges();
    sel.addRange(range);
  });
  expect(await selection(page)).toBe("line 2");

  handle.pushLiveFrame(altFrame(2));
  handle.pushLiveFrame(altFrame(3));
  await page.waitForTimeout(300);
  expect(await selection(page)).toBe("line 2");
  await expect(content(page)).toContainText("line 1");

  // Letting go releases the hold and the view catches up to the live edge.
  await page.evaluate(() => window.getSelection()?.removeAllRanges());
  handle.pushLiveFrame(altFrame(4));
  await expect.poll(() => content(page).textContent()).toContain("line 6");
});
