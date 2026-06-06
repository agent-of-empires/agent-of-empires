// Regression for #1345: clicking the sidebar resize bar (mousedown + mouseup,
// no drag) used to crash the whole app when localStorage was full because
// localStorage.setItem ran unguarded inside a React setState updater. The
// throw surfaced through the commit phase and blanked the dashboard.
//
// This spec stubs localStorage.setItem to throw QuotaExceededError for the
// sidebar width key only, then drives the exact click sequence the user
// reported. The app must stay mounted; the header must remain visible.
//
// The stub is enabled via a flag flipped just before the gesture so it does
// not interfere with page-load writes to unrelated keys.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

const SIDEBAR_WIDTH_KEY = "aoe-sidebar-width";
const SPLIT_STORAGE_KEY = "aoe-split-ratio";
const RIGHT_PANEL_KEY = "aoe-right-collapsed";

async function stubQuotaForKey(page: Page, key: string) {
  await page.addInitScript(
    ({ key: _key }) => {
      (window as unknown as { __throwQuotaFor?: Set<string> }).__throwQuotaFor =
        new Set();
      const original = Storage.prototype.setItem;
      Storage.prototype.setItem = function (k: string, v: string) {
        const throwSet = (
          window as unknown as { __throwQuotaFor?: Set<string> }
        ).__throwQuotaFor;
        if (throwSet && throwSet.has(k)) {
          throw new DOMException(
            "The quota has been exceeded.",
            "QuotaExceededError",
          );
        }
        return original.call(this, k, v);
      };
      (
        window as unknown as { __enableQuotaThrow: (k: string) => void }
      ).__enableQuotaThrow = (k: string) => {
        const set = (window as unknown as { __throwQuotaFor?: Set<string> })
          .__throwQuotaFor;
        if (set) set.add(k);
      };
    },
    { key },
  );
}

async function enableThrow(page: Page, key: string) {
  await page.evaluate((k) => {
    (
      window as unknown as { __enableQuotaThrow: (k: string) => void }
    ).__enableQuotaThrow(k);
  }, key);
}

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) =>
    r.fulfill({ json: { required: false, authenticated: true } }),
  );
  await page.route("**/api/sessions", (r) =>
    r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({ json: path === "docker/status" ? {} : [] }),
    );
  }
}

test.describe("#1345 localStorage QuotaExceeded crash regression", () => {
  test("sidebar resize bar click does not crash when setItem throws QuotaExceeded", async ({
    page,
  }) => {
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

  test("content split resize handle click does not crash when setItem throws QuotaExceeded", async ({
    page,
  }) => {
    await stubQuotaForKey(page, SPLIT_STORAGE_KEY);
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    await enableThrow(page, SPLIT_STORAGE_KEY);

    const handle = page.getByTestId("content-split-resize-handle");
    // Empty session list still renders the split; if not visible, this test
    // is a no-op for the build under test, not a regression.
    const count = await handle.count();
    if (count === 0) {
      test.skip(true, "content split not rendered with empty session list");
      return;
    }
    const box = await handle.first().boundingBox();
    if (!box) throw new Error("content-split handle has no bounding box");
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    await page.mouse.up();

    await expect(page.locator("header")).toBeVisible();
  });

  test("right-panel collapse toggle does not crash when setItem throws QuotaExceeded", async ({
    page,
  }) => {
    await stubQuotaForKey(page, RIGHT_PANEL_KEY);
    await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();

    await enableThrow(page, RIGHT_PANEL_KEY);

    // ControlOrMeta+Alt+B toggles diffCollapsed, which runs the guarded
    // useEffect that calls safeSetItem for RIGHT_PANEL_COLLAPSED_KEY.
    await page.locator("body").click();
    await page.keyboard.press("ControlOrMeta+Alt+B");
    await expect(page.locator("header")).toBeVisible();
  });
});
