import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, installTerminalSpies } from "./helpers/terminal-mocks";

// On WebKit (every iOS browser + desktop Safari) the WebGL addon must
// not load: Safari 26.x garbles xterm's WebGL glyph atlas
// (xtermjs/xterm.js#5816), which on an iPhone PWA rendered the terminal
// as a mostly-blank screen with giant mis-scaled glyphs. The iPhone 13
// device descriptor carries an iOS Safari user agent, so
// shouldUseWebglRenderer() must pick the DOM renderer here. The WebGL
// addon is the only thing that injects <canvas> elements into .xterm,
// so "no canvas" is a faithful proxy for "DOM renderer active".
test.use({ ...devices["iPhone 13"] });

test.describe("Mobile terminal renderer", () => {
  async function openSession(page: Page) {
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator(".xterm").waitFor({ state: "visible", timeout: 10_000 });
  }

  test("iPhone terminal uses the DOM renderer, not WebGL", async ({ page }) => {
    await installTerminalSpies(page);
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    // DOM renderer mounts the .xterm-rows tree and no canvases.
    await expect(page.locator(".xterm .xterm-rows")).toBeAttached();
    await expect(page.locator(".xterm canvas")).toHaveCount(0);
  });
});
