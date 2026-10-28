import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import type { MockHandle } from "./helpers/terminal-mocks";
import { expectScrollMode, liveMatches, liveTexts, openLiveTerminal, pushModeFrame } from "./helpers/liveTerminal";
import { DESKTOP } from "./helpers/viewports";

// The opt-in strip beside a full-screen mouse agent, driven through the real LiveTerminalView so its click-to-type
// ancestor and real pointer capture are in play (jsdom has neither).
test.describe("Live terminal forward scrollbar (desktop)", () => {
  test.use(DESKTOP);

  // Reports may share one socket message, so count reports, not messages.
  const WHEEL = /\x1b\[<6[45];\d+;\d+M/g;
  const wheels = (h: MockHandle) => liveTexts(h).reduce((n, s) => n + (s.match(WHEEL)?.length ?? 0), 0);

  async function mount(page: Page) {
    const handle = await openLiveTerminal(page, {
      settings: { desktopFontSize: 14, showForwardScrollbar: true },
    });
    await pushModeFrame(handle, { altScreen: true, mouse: true, mouseSgr: true });
    await expectScrollMode(page, "forward");
    await expect(page.getByRole("group", { name: "Scroll controls" })).toBeVisible();
    return handle;
  }

  test("Enter on an arrow twice keeps focus on it and sends only wheel reports", async ({ page }) => {
    const handle = await mount(page);
    const arrow = page.getByRole("button", { name: "Scroll down" });
    await arrow.focus();
    await page.keyboard.press("Enter");
    await page.keyboard.press("Enter");
    await expect.poll(() => wheels(handle)).toBeGreaterThanOrEqual(2);
    await expect(arrow).toBeFocused();
    // A typed Enter would put a carriage return on the wire.
    expect(liveMatches(handle, /\r/)).toBe(false);
  });

  test("a thumb drag keeps scrolling outside the strip and stops dead on release there", async ({ page }) => {
    const handle = await mount(page);
    const thumb = page.getByLabel("Drag to scroll");
    const box = (await thumb.boundingBox())!;
    const x = box.x + box.width / 2;
    const y = box.y + box.height / 2;

    await page.mouse.move(x, y);
    await page.mouse.down();
    // Far left of the strip and down the track: only pointer capture keeps the drag alive.
    await page.mouse.move(x - 300, y + 60, { steps: 5 });
    await expect.poll(() => wheels(handle)).toBeGreaterThan(0);

    await page.mouse.up();
    await expect(thumb).toHaveAttribute("style", /\+ 0px/);
    const settled = wheels(handle);
    await page.waitForTimeout(400);
    expect(wheels(handle)).toBe(settled);
  });
});
