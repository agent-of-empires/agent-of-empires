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
});
