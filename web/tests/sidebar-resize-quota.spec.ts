// #1345: clicking the sidebar resize bar with no drag crashed the app when
// localStorage was full, because setItem ran unguarded inside a React setState
// updater and the throw blanked the dashboard through the commit phase. The stub
// throws only for the sidebar width key, and only once the gesture starts, so
// page-load writes to other keys are unaffected.

import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";

const SIDEBAR_WIDTH_KEY = "aoe-sidebar-width";
const RIGHT_PANEL_KEY = "aoe-pane-layout";

async function stubQuotaForKey(page: Page, key: string) {
  await page.addInitScript(
    ({ key: _key }) => {
      (window as unknown as { __throwQuotaFor?: Set<string> }).__throwQuotaFor = new Set();
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (k: string, v: string) {
        const throwSet = (window as unknown as { __throwQuotaFor?: Set<string> }).__throwQuotaFor;
        if (throwSet && throwSet.has(k)) {
          throw new DOMException("The quota has been exceeded.", "QuotaExceededError");
        }
        return original.call(this, k, v);
      };
      (window as unknown as { __enableQuotaThrow: (k: string) => void }).__enableQuotaThrow = (k: string) => {
        const set = (window as unknown as { __throwQuotaFor?: Set<string> }).__throwQuotaFor;
        if (set) set.add(k);
      };
    },
    { key },
  );
}

async function enableThrow(page: Page, key: string) {
  await page.evaluate((k) => {
    (window as unknown as { __enableQuotaThrow: (k: string) => void }).__enableQuotaThrow(k);
  }, key);
}

async function mockApis(page: Page) {
  await mockStaticApis(page);
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
}

test.describe("#1345 localStorage QuotaExceeded crash regression", () => {
  test("sidebar resize bar click does not crash when setItem throws QuotaExceeded", async ({ page }) => {
    await stubQuotaForKey(page, SIDEBAR_WIDTH_KEY);
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    // Arm the quota throw AFTER page load so app-startup writes succeed.
    await enableThrow(page, SIDEBAR_WIDTH_KEY);

    const handle = page.getByTestId("sidebar-resize-handle");
    await expect(handle).toBeVisible();

    // Reproduce the exact reported gesture: mousedown + mouseup with no drag.
    // The previous bug fired localStorage.setItem unguarded inside a setState
    // updater, surfacing through the React commit phase.
    const box = await handle.boundingBox();
    if (!box) throw new Error("resize handle has no bounding box");
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.up();

    // App stayed mounted. If the fix regresses, the React tree blanks and
    // the header detaches from the DOM.
    await expect(page.locator("header")).toBeVisible();
  });

  test("right-panel collapse toggle does not crash when setItem throws QuotaExceeded", async ({ page }) => {
    await stubQuotaForKey(page, RIGHT_PANEL_KEY);
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    await enableThrow(page, RIGHT_PANEL_KEY);

    // ControlOrMeta+Alt+B toggles the diff pane, which runs the guarded
    // useEffect that calls safeSetItem for the pane-layout key.
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+Alt+B");
    await expect(page.locator("header")).toBeVisible();
  });
});
