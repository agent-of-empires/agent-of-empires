import { test, expect, observeFor } from "./helpers/mockedTest";
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

  test("a Backspace tap sends one DEL and holding it sends one DEL per autorepeat tick", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    // A single native edit must not be forwarded twice.
    const start = handle.liveMessages.length;
    await fireDeleteBackward(page, 1);
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(1);
    await observeFor(page, 200, async () => {
      expect(delCount(handle, start)).toBe(1);
    });

    // Core bug: pre-fix a held stream produced a single DEL (or none); post-fix
    // each tick maps to one DEL.
    await fireDeleteBackward(page, 5);
    await expect.poll(() => delCount(handle, start), { timeout: 5_000 }).toBe(6);
  });

  test("Backspace during IME composition is not forwarded", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.liveMessages.length;
    await fireDeleteBackward(page, 3, true);
    // A subsequent real input byte crosses the same ordered socket, proving
    // delivery of anything the composing events could have published.
    await page.locator('textarea[aria-label="Live terminal input"]').press("ArrowRight");
    await expect.poll(() => handle.liveInput.map((input) => input.toString())).toContain("\x1b[C");

    // The IME owns composing edits; the terminal must not inject DELs.
    await observeFor(page, 200, async () => {
      expect(delCount(handle, start)).toBe(0);
    });
  });
});
