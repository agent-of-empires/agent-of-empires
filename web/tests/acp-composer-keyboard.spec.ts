import { test, expect } from "./helpers/mockedTest";
import { mockStructuredSessionApis, openStructuredViewFor } from "./helpers/structuredSessionMocks";
import { devices, type Page } from "@playwright/test";

// #2011: on iOS regular Safari the layout viewport does not shrink when the
// soft keyboard opens, so the composer footer stayed pinned behind it. The fix
// reserves `keyboardHeight` as bottom padding on the structured-view root. Where
// innerHeight does shrink (iOS PWA, iOS 26 Safari, Android Chrome) that height
// is 0 and the dvh path is untouched.

test.use({ ...devices["iPhone 13"] });

const SESSION_ID = "sess-acp-kbd";
const TITLE = "acp-kbd";

async function setup(page: Page) {
  await mockStructuredSessionApis(page, { id: SESSION_ID, title: TITLE, projectPath: "/tmp/acp-kbd" });
}

const openStructuredSession = (page: Page) => openStructuredViewFor(page, TITLE);

// Override visualViewport.height (and optionally innerHeight) to mimic the soft
// keyboard, then fire the resize the hook listens for.
async function simulateKeyboardOpen(page: Page, keyboardPx: number, opts: { innerHeightShrinks?: boolean } = {}) {
  await page.evaluate(
    ({ keyboardPx, shrinkInner }) => {
      const vv = window.visualViewport;
      if (!vv) return;
      const newVvH = window.innerHeight - keyboardPx;
      Object.defineProperty(vv, "height", {
        get: () => newVvH,
        configurable: true,
      });
      Object.defineProperty(vv, "offsetTop", {
        get: () => 0,
        configurable: true,
      });
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

async function rootPaddingBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const root = document.querySelector<HTMLElement>('[data-testid="structured-view-root"]');
    return parseInt(root?.style.paddingBottom || "0") || 0;
  });
}

test.describe("Structured-view composer keyboard reservation (#2011)", () => {
  test("reserves keyboard height on iOS Safari, but not once the layout viewport shrinks (PWA / Android)", async ({
    page,
  }) => {
    await setup(page);
    await openStructuredSession(page);
    // No keyboard: the root carries no bottom reservation.
    expect(await rootPaddingBottom(page)).toBe(0);

    const root = await page.getByTestId("structured-view-root").elementHandle();
    expect(root).not.toBeNull();
    const reservation = () =>
      root!.evaluate((element) => ({
        connected: element.isConnected,
        padding: parseInt(element.style.paddingBottom || "0") || 0,
      }));
    // iOS regular Safari: visualViewport shrinks but innerHeight stays full, so the
    // root reserves ~keyboard height and the composer lifts above the keyboard.
    await simulateKeyboardOpen(page, 300);
    await expect.poll(async () => (await reservation()).padding).toBeGreaterThanOrEqual(250);
    await simulateKeyboardOpen(page, 300, { innerHeightShrinks: true });
    await expect.poll(reservation).toEqual({ connected: true, padding: 0 });
  });
});
