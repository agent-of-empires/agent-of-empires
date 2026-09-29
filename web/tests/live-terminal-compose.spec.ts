import { test, expect, observeFor } from "./helpers/mockedTest";
import type { MockHandle } from "./helpers/terminal-mocks";
import { openLiveTerminal } from "./helpers/liveTerminal";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { seedSettings } from "./helpers/terminal-mocks";
import { iPhone13 } from "./helpers/viewports";

const pastes = (handle: MockHandle) =>
  handle.liveMessages.flatMap((message) => {
    try {
      const parsed = JSON.parse(message.toString()) as { type?: string };
      return parsed.type === "paste" ? [parsed] : [];
    } catch {
      return [];
    }
  });

test.describe("Live terminal mobile controls", () => {
  test.use(iPhone13);

  test("typing stays in the sheet, and Send pastes once and hands focus back to the terminal", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });
    const typedBefore = handle.liveInput.length;

    await page.getByRole("button", { name: "Compose" }).tap();
    const message = page.getByRole("textbox", { name: "Message" });
    await expect(message).toBeFocused();
    await page.keyboard.type("first line");
    await page.keyboard.press("Enter");
    await page.keyboard.type("second");
    await page.getByRole("button", { name: "Send" }).tap();

    await expect(page.getByRole("dialog", { name: "Compose" })).toHaveCount(0);
    await expect(page.getByLabel("Live terminal input")).toBeFocused();
    await expect.poll(() => pastes(handle)).toEqual([{ type: "paste", text: "first line\nsecond", submit: true }]);
    expect(handle.liveInput.length).toBe(typedBefore);
  });

  test("the default key row fits one line, and an eight-key row still does not scroll", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });
    const row = ["Escape", "Tab", "Ctrl", "Paste from clipboard", "Compose"];
    for (const name of row) await expect(page.getByRole("button", { name, exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Tab", exact: true }).tap();
    await expect.poll(() => handle.liveInput.map((b) => b.toString())).toContain("\t");

    await seedSettings(page, {
      mobileToolbarKeys: ["esc", "tab", "shift-tab", "ctrl", "backspace", "enter", "paste", "compose"],
    });
    await page.reload();
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    const enter = page.getByRole("button", { name: "Enter", exact: true });
    await expect(enter).toBeVisible();
    expect((await enter.boundingBox())!.width).toBeGreaterThanOrEqual(40);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  });

  test("the key row sits low and inset from the corners without a keyboard, and keeps the full inset with one", async ({
    page,
  }) => {
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setSafeAreaInsetsOverride" as never, { insets: { bottom: 34 } } as never);
    await openLiveTerminal(page, { mobile: true });
    const esc = page.getByRole("button", { name: "Escape" });
    const layout = () =>
      esc.evaluate((key) => {
        const bar = key.parentElement!.getBoundingClientRect();
        const cap = key.getBoundingClientRect();
        return {
          barGap: innerHeight - bar.bottom,
          keyGap: innerHeight - cap.bottom,
          left: cap.left,
          height: cap.height,
        };
      });
    // 34 - 22 below the keys, and 0.7 * 34 in from each corner.
    const closed = await layout();
    expect(closed).toMatchObject({ barGap: 0, keyGap: 12, height: 36 });
    expect(closed.left).toBeCloseTo(23.8, 0);

    await page.getByLabel("Live terminal input").focus();
    await expect.poll(async () => (await layout()).keyGap).toBe(40);
    expect(await layout()).toMatchObject({ barGap: 0, left: 8, height: 40 });
  });

  test("dragging the joystick sends arrows and never opens the sidebar", async ({ page }) => {
    const handle = await openLiveTerminal(page, { mobile: true });
    const pad = page.getByRole("group", { name: "Arrow keys joystick" });
    const box = (await pad.boundingBox())!;
    const cx = box.x + box.width / 2;
    const cy = box.y + box.height / 2;
    const cdp = await page.context().newCDPSession(page);
    const touch = (type: string, x: number, y: number) =>
      cdp.send("Input.dispatchTouchEvent", {
        type,
        touchPoints: type === "touchEnd" ? [] : [{ x, y }],
      } as never);
    const typedBefore = handle.liveInput.length;
    await touch("touchStart", cx, cy);
    for (const step of [4, 8, 16, 24, 30]) await touch("touchMove", cx - step, cy);
    await touch("touchEnd", cx - 30, cy);
    await expect.poll(() => handle.liveInput.slice(typedBefore).map((b) => b.toString())).toContain("\x1b[D");

    // A swipe this long opens the sidebar from the terminal, but not when it runs over the joystick.
    const sidebarOpen = () =>
      page.evaluate(() => {
        const r = document.querySelector('[data-testid="sidebar-session-row"]')?.getBoundingClientRect();
        return !!r && r.x >= 0 && r.width > 0;
      });
    const swipeRight = (selector: string) =>
      page.evaluate((selector) => {
        const target = document.querySelector<HTMLElement>(selector)!;
        const fire = (type: string, x: number) => {
          const t = new Touch({ identifier: 7, target, clientX: x, clientY: 400 });
          const lifted = type === "touchend";
          target.dispatchEvent(
            new TouchEvent(type, {
              bubbles: true,
              cancelable: true,
              touches: lifted ? [] : [t],
              changedTouches: [t],
            }),
          );
        };
        fire("touchstart", 60);
        fire("touchmove", 120);
        fire("touchmove", 200);
        fire("touchend", 200);
      }, selector);
    await swipeRight("[data-arrow-joystick]");
    await observeFor(page, 400, async () => expect(await sidebarOpen()).toBe(false));
    await swipeRight("[data-live-terminal] > div");
    await expect.poll(sidebarOpen).toBe(true);
  });
});
