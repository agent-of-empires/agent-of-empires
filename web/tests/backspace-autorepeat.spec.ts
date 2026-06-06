import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { mockTerminalApis, type MockHandle } from "./helpers/terminal-mocks";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// Mobile soft-keyboard Backspace autorepeat arrives as a stream of
// `beforeinput` events (inputType "deleteContentBackward") rather than the
// repeated `keydown` autorepeat desktop OSes deliver. xterm decodes only the
// first into a single onData, so the PTY used to receive one DEL no matter how
// long Backspace was held. useTerminal now intercepts `beforeinput` on the
// hidden textarea and emits one DEL (0x7f) per tick, gated to coarse-pointer
// devices. See #1450.

// Count the lone 0x7f bytes our handler emits, ignoring the JSON control
// messages (activate / resize) that share the WS.
function delCount(handle: MockHandle, start: number) {
  return handle.wsMessages
    .slice(start)
    .map((msg) => msg.toString("utf8"))
    .filter((s) => s === "\x7f").length;
}

// Dispatch N `beforeinput` ticks on xterm's real hidden textarea, mirroring a
// held soft-keyboard Backspace. `isComposing` lets the IME case opt in.
async function fireDeleteBackward(
  page: Page,
  count: number,
  isComposing = false,
) {
  await page.evaluate(
    ({ count, isComposing }) => {
      const ta = document.querySelector<HTMLTextAreaElement>(".xterm textarea");
      if (!ta) throw new Error("xterm textarea not found");
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
const { defaultBrowserType: _iphoneBrowser, ...iPhone13 } =
  devices["iPhone 13"];

test.describe("Mobile soft-keyboard Backspace autorepeat", () => {
  test.use(iPhone13);

  async function openSession(page: Page, handle: MockHandle) {
    await page.goto("/");
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    // Desktop renders two `.xterm` nodes (agent + paired); scope to the first
    // to avoid a strict-mode locator violation.
    await page
      .locator(".xterm")
      .first()
      .waitFor({ state: "visible", timeout: 10_000 });
    await expect
      .poll(() => handle.wsMessages.length, { timeout: 5_000 })
      .toBeGreaterThan(0);
  }

  test("holding Backspace sends one DEL per autorepeat tick", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await fireDeleteBackward(page, 5);

    // Core bug: pre-fix this stream produced a single DEL (or none); post-fix
    // each tick maps to one DEL.
    await expect
      .poll(() => delCount(handle, start), { timeout: 5_000 })
      .toBe(5);
  });

  test("single Backspace tap sends exactly one DEL (no double-delete)", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await fireDeleteBackward(page, 1);

    // Regression guard: if preventDefault failed to suppress xterm's own
    // decode, a single tap would emit two DELs. Settle, then assert exactly 1.
    await expect
      .poll(() => delCount(handle, start), { timeout: 5_000 })
      .toBe(1);
    await page.waitForTimeout(200);
    expect(delCount(handle, start)).toBe(1);
  });

  test("Backspace during IME composition is left to xterm", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await openSession(page, handle);

    const start = handle.wsMessages.length;
    await fireDeleteBackward(page, 3, true);

    // isComposing ticks belong to xterm's composition path; our handler must
    // not inject DELs.
    await page.waitForTimeout(200);
    expect(delCount(handle, start)).toBe(0);
  });
});

test.describe("Desktop Backspace path unchanged", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("fine-pointer beforeinput does not trigger the mobile handler", async ({
    page,
  }) => {
    const handle = await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    // Desktop renders two `.xterm` nodes (agent + paired); scope to the first
    // to avoid a strict-mode locator violation.
    await page
      .locator(".xterm")
      .first()
      .waitFor({ state: "visible", timeout: 10_000 });
    await expect
      .poll(() => handle.wsMessages.length, { timeout: 5_000 })
      .toBeGreaterThan(0);

    const start = handle.wsMessages.length;
    await fireDeleteBackward(page, 5);

    // On a fine pointer the coarse-pointer gate is closed, so the handler is
    // inert; the real desktop delete path runs through xterm's keydown decode.
    await page.waitForTimeout(200);
    expect(delCount(handle, start)).toBe(0);
  });
});
