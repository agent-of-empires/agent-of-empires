import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// Mobile soft-keyboard Backspace autorepeat arrives as a stream of
// `beforeinput` events (inputType "deleteContentBackward") rather than the
// repeated `keydown` autorepeat desktop OSes deliver. The mobile live view
// listens for native `beforeinput` on its hidden input and emits one DEL
// (0x7f) per tick; composition events are left to the IME until
// compositionend. See #1450 for the original xterm-era bug.

// Count the lone 0x7f bytes our handler emits, ignoring the JSON control
// messages (resize / window / cadence) that share the WS.
function delCount(handle: MockHandle, start: number) {
  return handle.liveMessages
    .slice(start)
    .map((msg) => msg.toString("utf8"))
    .filter((s) => s === "\x7f").length;
}

// Dispatch N `beforeinput` ticks on the live view's hidden input,
// mirroring a held soft-keyboard Backspace. `isComposing` lets the IME
// case opt in.
async function fireDeleteBackward(page: Page, count: number, isComposing = false) {
  await page.evaluate(
    ({ count, isComposing }) => {
      const ta = document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Live terminal input"]');
      if (!ta) throw new Error("live terminal input not found");
      ta.focus();
      for (let i = 0; i < count; i++) {
        const evt = new InputEvent("beforeinput", {
          inputType: "deleteContentBackward",
          bubbles: true,
          cancelable: true,
        });
        // isComposing is readonly on the constructed event; force it for the
        // IME path so the handler's `e.isComposing` guard is exercised.
        if (isComposing) {
          Object.defineProperty(evt, "isComposing", { get: () => true });
        }
        ta.dispatchEvent(evt);
      }
    },
    { count, isComposing },
  );
}

// Drop `defaultBrowserType` from the device profile: Playwright forbids it in
// a describe-level test.use (it would force a new worker), and the project
// already pins chromium. We only want the iPhone 13 viewport / touch / UA so
// `(pointer: coarse)` matches.
const { defaultBrowserType: _iphoneBrowser, ...iPhone13 } = devices["iPhone 13"];

test.describe("Mobile soft-keyboard Backspace autorepeat", () => {
  test.use(iPhone13);

  async function openSession(page: Page, handle: MockHandle) {
    await page.goto("/");
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
    await expect.poll(() => handle.liveMessages.length, { timeout: 5_000 }).toBeGreaterThan(0);
  }

  test("holding Backspace sends one DEL per autorepeat tick", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await fireDeleteBackward(page, 5);

    // Core bug: pre-fix this stream produced a single DEL (or none); post-fix
    // each tick maps to one DEL.
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(5);
  });

  test("single Backspace tap sends exactly one DEL (no double-delete)", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await fireDeleteBackward(page, 1);

    // Regression guard: if preventDefault failed to suppress xterm's own
    // decode, a single tap would emit two DELs. Settle, then assert exactly 1.
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(1);
    await page.waitForTimeout(200);
    expect(delCount(handle, start)).toBe(1);
  });

  test("Backspace during IME composition is left to xterm", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await fireDeleteBackward(page, 3, true);

    // isComposing ticks belong to xterm's composition path; our handler must
    // not inject DELs.
    await page.waitForTimeout(200);
    expect(delCount(handle, start)).toBe(0);
  });
});

test.describe("Desktop uses the unified live view", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("desktop renders the live input (no xterm); the touch toolbar is hidden", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });

    // Unified renderer: the live view + its input mount everywhere; xterm is
    // gone. The soft-keyboard toolbar/FAB are touch-only, hidden on desktop.
    await expect(page.locator(".xterm")).toHaveCount(0);
    await expect(page.locator('textarea[aria-label="Live terminal input"]').first()).toBeAttached();
    await expect(page.getByRole("button", { name: "Backspace" })).toHaveCount(0);
  });
});
