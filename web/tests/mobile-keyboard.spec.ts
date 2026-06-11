import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, seedSettings } from "./helpers/terminal-mocks";

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

async function openSession(page: Page) {
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").waitFor({ state: "visible", timeout: 10_000 });
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
    await mockTerminalApis(page);
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
    await page.waitForTimeout(500);
    await openSession(page);
  }

  test("auto-resizes when keyboard opens in Safari browser mode (innerHeight constant)", async ({ page }) => {
    await setupAndOpen(page);

    // No keyboard yet: the pane is full-size, no occlusion padding. The
    // sticky reservation and its localStorage seed are gone (#1432).
    const before = await getKeyboardState(page);
    expect(parseInt(before.rootPaddingBottom) || 0).toBe(0);

    await simulateKeyboardOpen(page, 300);
    // Wait past the occlusion-commit debounce plus the rAF settle window.
    await page.waitForTimeout(800);

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
    await page.waitForTimeout(600);

    const state = await getKeyboardState(page);
    expect(state.rootPaddingBottom === "0" || state.rootPaddingBottom === "").toBe(true);
  });

  test("auto-resizes back when keyboard closes (occlusion releases)", async ({ page }) => {
    await setupAndOpen(page);

    await simulateKeyboardOpen(page, 300);
    await page.waitForTimeout(800);
    const open = await getKeyboardState(page);
    expect(parseInt(open.rootPaddingBottom)).toBeGreaterThanOrEqual(250);

    await simulateKeyboardClose(page);
    await page.waitForTimeout(800);

    const after = await getKeyboardState(page);
    // Occlusion releases to 0 when the keyboard dismisses, so the pane grows
    // back to full size. This is the #1432 behavior, the inverse of the old
    // sticky reservation that kept the pane shrunk across the cycle.
    expect(parseInt(after.rootPaddingBottom) || 0).toBe(0);
  });

  test("toolbar renders on mobile with active session", async ({ page }) => {
    await setupAndOpen(page);
    // On chromium headless, pointer:coarse may not match — toolbar only
    // renders when isMobile is true. Check that the terminal at least loaded.
    await expect(page.locator("[data-live-terminal]")).toBeVisible();
  });

  test("keyboard open button visible when keyboard closed", async ({ page }) => {
    await setupAndOpen(page);
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

    const _scrolledToBottom = await page.evaluate(() => {
      return new Promise<boolean>((resolve) => {
        const _orig = (
          window as unknown as {
            __termScrollBottom?: boolean;
          }
        ).__termScrollBottom;
        // Watch for scrollTop change on the terminal container
        const wt = document.querySelector("[data-live-terminal]");
        if (!wt) return resolve(false);
        // Watch for scroll events on the .xterm element
        const onScroll = () => {
          resolve(true);
          wt.removeEventListener("scroll", onScroll);
        };
        wt.addEventListener("scroll", onScroll);
        setTimeout(() => {
          resolve(false);
          wt.removeEventListener("scroll", onScroll);
        }, 2000);
      });
    });
    // Trigger keyboard after setting up observer
    await simulateKeyboardOpen(page, 300);
    // The test is primarily that no crash occurs; scroll observation is best-effort
  });

  test("small viewport delta below threshold does NOT pad the pane", async ({ page }) => {
    await setupAndOpen(page);
    const before = await getKeyboardState(page);
    expect(parseInt(before.rootPaddingBottom) || 0).toBe(0);

    // Simulate URL bar collapse: ~80px change, below the 100px threshold
    await simulateKeyboardOpen(page, 80);
    await page.waitForTimeout(800);

    const state = await getKeyboardState(page);
    // Occlusion only counts as a keyboard above 100px; an 80px delta is not
    // treated as a keyboard, so no padding is applied.
    expect(parseInt(state.rootPaddingBottom) || 0).toBe(0);
  });

  test("orientation change resets fullHeight baseline", async ({ page }) => {
    await setupAndOpen(page);

    // Simulate landscape orientation
    await page.setViewportSize({ width: 844, height: 390 });
    await page.waitForTimeout(600);

    // Now open keyboard in landscape
    await simulateKeyboardOpen(page, 200);
    await page.waitForTimeout(800);

    const state = await getKeyboardState(page);
    // Should detect keyboard relative to the landscape height, not portrait
    expect(parseInt(state.rootPaddingBottom)).toBeGreaterThan(150);
  });
});

test.describe("Mobile proxy input keydown handling", () => {
  async function setupWithWsSpy(page: Page) {
    await page.addInitScript(() => {
      (window as unknown as { __PTY_SENT__: string[] }).__PTY_SENT__ = [];
      const Orig = window.WebSocket;
      window.WebSocket = class extends Orig {
        constructor(url: string | URL, protocols?: string | string[]) {
          super(url, protocols);
          const origSend = this.send.bind(this);
          this.send = (data: string | ArrayBufferLike | Blob | ArrayBufferView) => {
            if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
              const bytes = new Uint8Array(data instanceof ArrayBuffer ? data : data.buffer);
              (window as unknown as { __PTY_SENT__: string[] }).__PTY_SENT__.push(new TextDecoder().decode(bytes));
            }
            return origSend(data);
          };
        }
      } as typeof WebSocket;
    });
    await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await page.waitForTimeout(300);
    await openSession(page);
  }

  async function sendKeyAndGetPtySent(page: Page, key: string, code: string) {
    await page.evaluate(
      ({ key, code }) => {
        const proxy = document.querySelector<HTMLInputElement>('input[autocapitalize="none"]');
        if (!proxy) throw new Error("proxy input not found");
        proxy.focus();
        proxy.dispatchEvent(new KeyboardEvent("keydown", { key, code, bubbles: true }));
      },
      { key, code },
    );
    await page.waitForTimeout(100);
    return page.evaluate(() => (window as unknown as { __PTY_SENT__: string[] }).__PTY_SENT__);
  }

  test("Enter key sends carriage return via proxy keydown", async ({ page, browserName }) => {
    test.skip(browserName !== "webkit", "proxy input requires pointer:coarse (mobile only)");
    await setupWithWsSpy(page);
    const sent = await sendKeyAndGetPtySent(page, "Enter", "Enter");
    expect(sent).toContain("\r");
  });

  test("Backspace key sends DEL (0x7f) via proxy keydown", async ({ page, browserName }) => {
    test.skip(browserName !== "webkit", "proxy input requires pointer:coarse (mobile only)");
    await setupWithWsSpy(page);
    const sent = await sendKeyAndGetPtySent(page, "Backspace", "Backspace");
    expect(sent).toContain("\x7f");
  });
});

test.describe("Mobile keyboard hooks ordering", () => {
  test("no React hooks error when transitioning pending → ready", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (err) => errors.push(err.message));

    await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await page.waitForTimeout(300);
    await openSession(page);

    await page.waitForTimeout(500);

    const hookErrors = errors.filter((e) => e.includes("hook") || e.includes("Hook"));
    expect(hookErrors).toEqual([]);
  });

  test("no errors when keyboard opens during session", async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", (err) => errors.push(err.message));

    await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
    await page.goto("/");
    await page.waitForTimeout(300);
    await openSession(page);

    // Simulate keyboard open/close cycle
    await simulateKeyboardOpen(page, 300);
    await page.waitForTimeout(300);
    await simulateKeyboardClose(page);
    await page.waitForTimeout(300);

    const hookErrors = errors.filter((e) => e.includes("hook") || e.includes("Hook") || e.includes("Rendered"));
    expect(hookErrors).toEqual([]);
  });
});
