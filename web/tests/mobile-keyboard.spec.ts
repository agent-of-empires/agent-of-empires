import { test, expect, observeFor } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, seedSettings, type MockHandle } from "./helpers/terminal-mocks";

// Use iPhone 13 profile: pointer:coarse, hasTouch, correct viewport, WebKit UA.
test.use({ ...devices["iPhone 13"] });

// Simulate iOS soft keyboard opening by overriding visualViewport dimensions.
// In real iOS Safari, visualViewport.height shrinks while window.innerHeight
// may or may not (browser tab vs PWA). We test both scenarios.
async function simulateKeyboardOpen(page: Page, keyboardPx: number, opts: { innerHeightShrinks?: boolean } = {}) {
  await page.evaluate(
    ({ keyboardPx, shrinkInner }) => {
      const vv = window.visualViewport;
      if (!vv) return;
      const fullH = window.innerHeight;
      const newVvH = fullH - keyboardPx;

      // Override visualViewport.height via property descriptor
      Object.defineProperty(vv, "height", {
        get: () => newVvH,
        configurable: true,
      });
      Object.defineProperty(vv, "offsetTop", {
        get: () => 0,
        configurable: true,
      });

      // In PWA standalone mode, innerHeight shrinks WITH the keyboard
      if (shrinkInner) {
        Object.defineProperty(window, "innerHeight", {
          get: () => newVvH,
          configurable: true,
        });
      }

      vv.dispatchEvent(new Event("resize"));
    },
    { keyboardPx, shrinkInner: opts.innerHeightShrinks ?? false },
  );
}

async function simulateKeyboardClose(page: Page) {
  await page.evaluate(() => {
    const vv = window.visualViewport;
    if (!vv) return;

    // Restore original descriptors by deleting overrides
    const vvProto = Object.getPrototypeOf(vv);
    const origHeight = Object.getOwnPropertyDescriptor(vvProto, "height");
    const origOffset = Object.getOwnPropertyDescriptor(vvProto, "offsetTop");
    if (origHeight) Object.defineProperty(vv, "height", origHeight);
    else delete (vv as Record<string, unknown>)["height"];
    if (origOffset) Object.defineProperty(vv, "offsetTop", origOffset);
    else delete (vv as Record<string, unknown>)["offsetTop"];

    // Restore innerHeight
    const origInner = Object.getOwnPropertyDescriptor(Window.prototype, "innerHeight");
    if (origInner) Object.defineProperty(window, "innerHeight", origInner);

    vv.dispatchEvent(new Event("resize"));
  });
}

async function openSession(page: Page, handle: MockHandle) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
  await handle.waitForLiveReady();
}

async function getKeyboardState(page: Page) {
  return page.evaluate(() => {
    const root = document.querySelector<HTMLElement>('[class*="flex-1 flex flex-col overflow-hidden relative"]');
    const termContainer = document.querySelector<HTMLElement>("[data-live-terminal]");
    return {
      rootHeight: root?.getBoundingClientRect().height ?? 0,
      rootPaddingBottom: root?.style.paddingBottom || "0",
      termHeight: termContainer?.getBoundingClientRect().height ?? 0,
      innerHeight: window.innerHeight,
      vvHeight: Math.round(window.visualViewport?.height ?? 0),
    };
  });
}

test.describe("Mobile keyboard detection and layout", () => {
  async function setupAndOpen(page: Page) {
    // Mocks must be set up BEFORE any navigation so the initial API
    // requests are intercepted (especially /api/sessions).
    const handle = await mockTerminalApis(page);
    // ensureSession POSTs to /api/sessions/{id}/ensure
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    // seedSettings writes to localStorage (needs page loaded), then reload
    // so the app picks up the seeded settings with mocks still active.
    // This suite exercises keyboard detection / layout / FAB mechanics, not the
    // auto-open-on-select feature. Disable auto-open so the keyboard starts
    // closed deterministically (otherwise the select handler focuses the input
    // and the FAB would read "Close keyboard").
    await seedSettings(page, { mobileFontSize: 10, autoOpenKeyboard: false });
    await page.reload();
    await openSession(page, handle);
  }

  test("mobile shell is fixed and rejects document-level scroll", async ({ page }) => {
    await setupAndOpen(page);

    const result = await page.evaluate(() => {
      const calls: Array<[number, number]> = [];
      const original = window.scrollTo;
      Object.defineProperty(window, "scrollTo", {
        configurable: true,
        value: (x: number, y: number) => calls.push([x, y]),
      });
      Object.defineProperty(document.documentElement, "scrollTop", { configurable: true, value: 120 });
      window.dispatchEvent(new Event("scroll"));
      Object.defineProperty(window, "scrollTo", { configurable: true, value: original });
      return { calls, rootPosition: getComputedStyle(document.getElementById("root")!).position };
    });

    expect(result.rootPosition).toBe("fixed");
    expect(result.calls).toContainEqual([0, 0]);
  });

  test("auto-resizes when keyboard opens in Safari browser mode (innerHeight constant)", async ({ page }) => {
    await setupAndOpen(page);

    // No keyboard yet: the pane is full-size, no occlusion padding. The
    // sticky reservation and its localStorage seed are gone (#1432).
    const before = await getKeyboardState(page);
    expect(parseInt(before.rootPaddingBottom) || 0).toBe(0);

    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom))
      .toBeGreaterThanOrEqual(250);

    const after = await getKeyboardState(page);
    // The pane is padded by the live occlusion (~300) so the terminal shrinks.
    expect(parseInt(after.rootPaddingBottom)).toBeGreaterThanOrEqual(250);
  });

  test("PWA mode keyboard adds no inset (dvh shrink owns the layout)", async ({ page }) => {
    await setupAndOpen(page);

    // PWA / iOS 26 / Android: innerHeight shrinks with the keyboard, so
    // 100dvh shrinks the layout natively. The live view must not stack
    // its own inset on top (that would double-shrink), and the agent
    // pane root carries no inline padding in this mode. The dvh shrink
    // itself is not simulable here; the assertable part is that the
    // legacy occlusion machinery stays quiet.
    await simulateKeyboardOpen(page, 300, { innerHeightShrinks: true });
    await observeFor(page, 600, async () => {
      expect(parseInt((await getKeyboardState(page)).rootPaddingBottom) || 0).toBe(0);
    });

    const state = await getKeyboardState(page);
    expect(state.rootPaddingBottom === "0" || state.rootPaddingBottom === "").toBe(true);
  });

  test("auto-resizes back when keyboard closes (occlusion releases)", async ({ page }) => {
    await setupAndOpen(page);

    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom))
      .toBeGreaterThanOrEqual(250);
    const open = await getKeyboardState(page);
    expect(parseInt(open.rootPaddingBottom)).toBeGreaterThanOrEqual(250);

    await simulateKeyboardClose(page);
    await expect.poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom) || 0).toBe(0);

    const after = await getKeyboardState(page);
    // Occlusion releases to 0 when the keyboard dismisses, so the pane grows
    // back to full size. This is the #1432 behavior, the inverse of the old
    // sticky reservation that kept the pane shrunk across the cycle.
    expect(parseInt(after.rootPaddingBottom) || 0).toBe(0);
  });

  test("toolbar renders on mobile with active session", async ({ page }) => {
    await setupAndOpen(page);
    await expect(page.getByRole("button", { name: "Arrow up", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Ctrl+C interrupt", exact: true })).toBeVisible();
  });

  test("keyboard open button visible when keyboard closed", async ({ page }) => {
    await setupAndOpen(page);
    await expect(page.getByRole("button", { name: "Open keyboard" })).toBeVisible();
  });

  test("Claude terminal selection keeps the keyboard closed", async ({ page }) => {
    const handle = await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    // The fixture's terminal session uses tool: "claude". Its alternate-screen
    // startup still drops the first iOS keyboard input, so the selection must
    // remain usable as a monitoring view until the user opens the keyboard.
    await seedSettings(page, { mobileFontSize: 10, autoOpenKeyboard: true });
    await page.reload();
    await openSession(page, handle);
    await expect(page.getByRole("button", { name: "Open keyboard" })).toBeVisible();
  });

  test("keyboard FAB tracks input focus, not viewport heuristics", async ({ page }) => {
    await setupAndOpen(page);

    // On a touch device the keyboard is open exactly when the live input
    // has focus; the FAB icon follows focus directly, so no viewport
    // simulation is needed (or consulted).
    await expect(page.getByRole("button", { name: "Open keyboard" })).toBeVisible();
    await page.evaluate(() => {
      document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Live terminal input"]')?.focus();
    });
    await expect(page.getByRole("button", { name: "Close keyboard" })).toBeVisible();
    await page.evaluate(() => {
      document.querySelector<HTMLTextAreaElement>('textarea[aria-label="Live terminal input"]')?.blur();
    });
    await expect(page.getByRole("button", { name: "Open keyboard" })).toBeVisible();
  });

  test("scrollToBottom fires when keyboard opens", async ({ page }) => {
    await setupAndOpen(page);

    const scroller = page.locator("[data-live-terminal] > div").first();
    await scroller.evaluate((el) => {
      el.scrollTop = 0;
      el.dispatchEvent(new Event("scroll"));
    });
    await expect(page.getByRole("button", { name: "Back to live" })).toBeVisible();
    const before = await scroller.evaluate((el) => el.scrollTop);
    await simulateKeyboardOpen(page, 300);
    await expect.poll(() => scroller.evaluate((el) => el.scrollTop)).toBeGreaterThan(before);
    await expect(page.getByRole("button", { name: "Back to live" })).toBeHidden();
    await expect
      .poll(() =>
        scroller.evaluate((el) => {
          const cursor = el.querySelector<HTMLElement>("[data-live-cursor]");
          if (!cursor) return false;
          const pane = el.getBoundingClientRect(),
            rect = cursor.getBoundingClientRect();
          return rect.top >= pane.top - 2 && rect.bottom <= pane.bottom + 2;
        }),
      )
      .toBe(true);
  });

  test("small viewport delta below threshold does NOT pad the pane", async ({ page }) => {
    await setupAndOpen(page);
    const before = await getKeyboardState(page);
    expect(parseInt(before.rootPaddingBottom) || 0).toBe(0);

    // Simulate URL bar collapse: ~80px change, below the 100px threshold
    await simulateKeyboardOpen(page, 80);
    await observeFor(page, 800, async () => {
      expect(parseInt((await getKeyboardState(page)).rootPaddingBottom) || 0).toBe(0);
    });

    const state = await getKeyboardState(page);
    // Occlusion only counts as a keyboard above 100px; an 80px delta is not
    // treated as a keyboard, so no padding is applied.
    expect(parseInt(state.rootPaddingBottom) || 0).toBe(0);
  });

  test("orientation change resets fullHeight baseline", async ({ page }) => {
    await setupAndOpen(page);

    // Simulate landscape orientation
    await page.setViewportSize({ width: 844, height: 390 });
    await expect
      .poll(() => page.locator("[data-live-terminal]").evaluate((el) => el.getBoundingClientRect().height))
      .toBeLessThan(390);

    // Now open keyboard in landscape
    await simulateKeyboardOpen(page, 200);
    await expect.poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom)).toBeGreaterThan(150);

    const state = await getKeyboardState(page);
    // Should detect keyboard relative to the landscape height, not portrait
    expect(parseInt(state.rootPaddingBottom)).toBeGreaterThan(150);
  });
});

test.describe("Mobile proxy input keydown handling", () => {
  async function setupProxySession(page: Page) {
    const handle = await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await openSession(page, handle);
    return handle;
  }

  async function sendProxyKey(page: Page, key: string, code: string) {
    await page.evaluate(
      ({ key, code }) => {
        const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
        if (!proxy) throw new Error("proxy input not found");
        proxy.focus();
        proxy.dispatchEvent(new KeyboardEvent("keydown", { key, code, bubbles: true }));
      },
      { key, code },
    );
  }

  test("Enter key sends carriage return via proxy keydown", async ({ page }) => {
    const handle = await setupProxySession(page);
    await sendProxyKey(page, "Enter", "Enter");
    await expect.poll(() => handle.liveInput.map((input) => input.toString())).toContain("\r");
  });

  test("Backspace key sends DEL (0x7f) via proxy keydown", async ({ page }) => {
    const handle = await setupProxySession(page);
    await sendProxyKey(page, "Backspace", "Backspace");
    await expect.poll(() => handle.liveInput.map((input) => input.toString())).toContain("\x7f");
  });

  test("reselecting the active session preserves keyboard-proxy input", async ({ page }) => {
    const terminal = await mockTerminalApis(page, { tool: "codex" });
    await page.goto("/");
    await openSession(page, terminal);

    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });

    const input = await page.evaluate(() => {
      const proxy = document.querySelector<HTMLTextAreaElement>("[data-keyboard-proxy]");
      if (!proxy) throw new Error("keyboard proxy not found");
      const event = new InputEvent("beforeinput", {
        bubbles: true,
        cancelable: true,
        inputType: "insertText",
        data: "reselected",
      });
      const delivered = proxy.dispatchEvent(event);
      return { data: event.data, delivered, inputType: event.inputType };
    });
    // `delivered` is dispatchEvent's return: true means the default was NOT
    // prevented, so the text also lands in the proxy textarea as IME context
    // (forwardTerminalBeforeInput).
    expect(input).toEqual({ data: "reselected", delivered: true, inputType: "insertText" });
    await expect
      .poll(() => terminal.liveMessages.map((message) => message.toString()).join("\n"))
      .toContain("reselected");
  });
});

test.describe("Mobile keyboard hooks ordering", () => {
  test("no React hooks error when transitioning pending → ready", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (err) => errors.push(err.message));

    const handle = await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await openSession(page, handle);

    const hookErrors = errors.filter((e) => e.includes("hook") || e.includes("Hook"));
    expect(hookErrors).toEqual([]);
  });

  test("no errors when keyboard opens during session", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (err) => errors.push(err.message));

    const handle = await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await openSession(page, handle);

    // Observe both layout transitions before inspecting asynchronous errors.
    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom))
      .toBeGreaterThanOrEqual(250);
    await simulateKeyboardClose(page);
    await expect.poll(async () => parseInt((await getKeyboardState(page)).rootPaddingBottom) || 0).toBe(0);

    const hookErrors = errors.filter((e) => e.includes("hook") || e.includes("Hook") || e.includes("Rendered"));
    expect(hookErrors).toEqual([]);
  });
});
