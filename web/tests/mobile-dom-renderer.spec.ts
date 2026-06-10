import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, installTerminalSpies } from "./helpers/terminal-mocks";

// On touch-primary devices the agent pane renders the capture-snapshot
// live view (the TUI's live-mode architecture): real DOM text, native
// scrolling, no xterm.js and no canvases at all. This sidesteps the
// WebKit WebGL corruption (xtermjs/xterm.js#5816) that motivated the
// earlier DOM-renderer gate, and gives iOS native text selection.
test.use({ ...devices["iPhone 13"] });

test.describe("Mobile terminal renderer", () => {
  async function openSession(page: Page) {
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  }

  test("iPhone agent pane renders the live view, not xterm", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    // Live view: DOM rows, zero canvases, no xterm mount.
    await expect(page.locator("[data-live-content]")).toBeAttached();
    await expect(page.locator("[data-live-terminal] canvas")).toHaveCount(0);
    await expect(page.locator(".xterm")).toHaveCount(0);
    // Frame content is real selectable text.
    await expect.poll(() => page.locator("[data-live-content]").innerText()).toContain("$ ready");
  });
});
