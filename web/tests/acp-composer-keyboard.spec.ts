import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// Mobile keyboard regression for the structured-view composer (#2011).
//
// On iOS regular Safari the layout viewport does NOT shrink when the soft
// keyboard opens (interactive-widget=resizes-content is Chromium/Android only,
// and dvh does not track the iOS keyboard), so the composer footer was left
// pinned to the full-height bottom edge, hidden behind the keyboard. The fix
// reserves `keyboardHeight` as bottom padding on the structured-view root so
// the chat viewport absorbs the shrink and the composer rises above the
// keyboard. On platforms where innerHeight shrinks with the keyboard (iOS PWA,
// iOS 26 Safari, Android Chrome) `keyboardHeight` is 0, so the reservation is a
// no-op and the existing dvh path is untouched.
//
// We render the structured view in mocked mode (one running ACP session, no
// live agent) and drive the iOS keyboard by overriding visualViewport, the
// same technique as mobile-keyboard.spec.ts.

test.use({ ...devices["iPhone 13"] });

const SESSION_ID = "sess-acp-kbd";
const TITLE = "acp-kbd";

async function setup(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of [
    "settings",
    "themes",
    "agents",
    "profiles",
    "groups",
    "devices",
    "docker/status",
    "about",
    "system/update-status",
  ]) {
    await page.route(`**/api/${path}`, (r) =>
      r.fulfill({
        json:
          path === "docker/status" || path === "about" || path === "settings" || path === "system/update-status"
            ? {}
            : [],
      }),
    );
  }
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: SESSION_ID,
            title: TITLE,
            project_path: "/tmp/acp-kbd",
            group_path: "/tmp",
            tool: "claude",
            status: "Running",
            yolo_mode: false,
            created_at: new Date().toISOString(),
            last_accessed_at: null,
            last_error: null,
            branch: null,
            main_repo_path: null,
            is_sandboxed: false,
            has_terminal: true,
            profile: "default",
            workspace_repos: [],
            view: "structured",
            acp_worker_state: "running",
            claude_fullscreen: false,
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions/*/acp/**", (r) => r.fulfill({ json: {} }));
  await page.routeWebSocket(/\/sessions\/[^/]+\/ws(\?|$)/, () => {});
  await page.routeWebSocket(/\/sessions\/[^/]+\/acp\/ws/, () => {});
}

async function openStructuredSession(page: Page) {
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  // On a mobile viewport the sidebar is collapsed behind a toggle; open it
  // before the session link is reachable.
  await openMobileSidebar(page);
  await clickSidebarSession(page, TITLE);
  await expect(page.getByTestId("structured-view-root")).toBeVisible({
    timeout: 10000,
  });
}

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
  test("reserves keyboard height so the composer clears the keyboard on iOS Safari (innerHeight constant)", async ({
    page,
  }) => {
    await setup(page);
    await openStructuredSession(page);

    // No keyboard: the root carries no bottom reservation.
    expect(await rootPaddingBottom(page)).toBe(0);

    // iOS regular Safari: visualViewport shrinks but innerHeight stays full.
    await simulateKeyboardOpen(page, 300);
    await page.waitForTimeout(400);

    // The root reserves ~keyboard height so the flex-1 viewport shrinks and the
    // composer lifts above the keyboard.
    expect(await rootPaddingBottom(page)).toBeGreaterThanOrEqual(250);
  });

  test("does NOT reserve when the layout viewport already shrinks (PWA / Android, innerHeight shrinks)", async ({
    page,
  }) => {
    await setup(page);
    await openStructuredSession(page);

    expect(await rootPaddingBottom(page)).toBe(0);

    // innerHeight shrinks with the keyboard: keyboardHeight is 0, dvh handles it.
    await simulateKeyboardOpen(page, 300, { innerHeightShrinks: true });
    await page.waitForTimeout(400);

    expect(await rootPaddingBottom(page)).toBe(0);
  });
});
