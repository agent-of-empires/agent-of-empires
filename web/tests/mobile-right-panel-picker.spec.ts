// Regression for #1452: on mobile the right panel used to be an 85vw
// slide-in overlay pinned to bottom-0. When the soft keyboard opened
// against the paired terminal inside it, the live keyboardHeight padding
// collapsed the terminal to near-zero height. The fix replaces the overlay
// with a picker that promotes the chosen view into the single full-viewport
// main pane, so the paired terminal owns the viewport and stays tall under
// the keyboard (the same posture the agent terminal already uses).

import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis, seedSettings } from "./helpers/terminal-mocks";

// iPhone 13: width 390 (< md), pointer:coarse, hasTouch, WebKit UA.
test.use({ ...devices["iPhone 13"] });

// Override visualViewport to model the iOS soft keyboard occluding the
// bottom of the layout viewport.
async function simulateKeyboardOpen(page: Page, keyboardPx: number) {
  await page.evaluate((keyboardPx) => {
    const vv = window.visualViewport;
    if (!vv) return;
    const newVvH = window.innerHeight - keyboardPx;
    Object.defineProperty(vv, "height", {
      get: () => newVvH,
      configurable: true,
    });
    Object.defineProperty(vv, "offsetTop", { get: () => 0, configurable: true });
    vv.dispatchEvent(new Event("resize"));
  }, keyboardPx);
}

async function setupAndOpenSession(page: Page) {
  await mockTerminalApis(page);
  await page.goto("/");
  await seedSettings(page, { mobileFontSize: 10 });
  await page.reload();
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });
}

async function openPicker(page: Page) {
  await page.getByRole("button", { name: "Toggle diff panel" }).click();
  await page.getByTestId("mobile-right-panel-picker").waitFor({
    state: "visible",
    timeout: 5_000,
  });
}

test.describe("Mobile right panel picker (#1452)", () => {
  test("picker promotes the paired terminal and it survives the keyboard", async ({
    page,
  }) => {
    await setupAndOpenSession(page);
    await openPicker(page);

    await page.getByTestId("mobile-right-panel-pick-paired").click();
    // Picker closes; the paired shell mounts at full viewport.
    await expect(page.getByTestId("mobile-right-panel-picker")).toHaveCount(0);
    const paired = page.locator('[data-term="paired"]');
    await paired.waitFor({ state: "visible", timeout: 10_000 });

    // The bug: keyboard padding collapsed the paired terminal to ~0px.
    // Now it owns the viewport, so it stays comfortably tall.
    await simulateKeyboardOpen(page, 300);
    // Poll the height rather than sleeping a fixed time: the layout settles
    // a frame or two after the visualViewport resize.
    await expect
      .poll(async () => (await paired.boundingBox())?.height ?? 0, {
        message: "paired terminal collapsed under the keyboard",
      })
      .toBeGreaterThan(150);
  });

  test("picker promotes the diff view, opens a file, and the back chip returns to the agent", async ({
    page,
  }) => {
    await mockTerminalApis(page);
    // Seed one changed file so the diff list has a row to tap; tapping it
    // promotes the full-screen file viewer into the same pane.
    await page.route("**/api/sessions/*/diff/files", (r) =>
      r.fulfill({
        json: {
          files: [
            {
              path: "src/foo.ts",
              old_path: null,
              status: "modified",
              additions: 2,
              deletions: 1,
            },
          ],
          per_repo_bases: [{ base_branch: "main" }],
          warning: null,
        },
      }),
    );
    await page.goto("/");
    await openMobileSidebar(page);
    await clickSidebarSession(page, "pinch-test");
    await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });

    await openPicker(page);
    await page.getByTestId("mobile-right-panel-pick-diff").click();
    await expect(page.getByTestId("mobile-right-panel-picker")).toHaveCount(0);
    // The non-structured views carry a persistent back affordance.
    const back = page.getByTestId("mobile-back-to-agent");
    await expect(back).toBeVisible();

    // Tap the file row to promote the diff viewer in place; the viewer
    // replaces the file list in the same pane, so the row disappears.
    const row = page.locator('button[data-index="0"]').first();
    await row.hover();
    await row.click();
    await expect(page.locator('button[data-index="0"]')).toHaveCount(0);
    await expect(back).toBeVisible();

    await back.click();
    await expect(page.getByTestId("mobile-back-to-agent")).toHaveCount(0);
    await expect(page.locator(".xterm").first()).toBeVisible();
  });

  test("agent and paired terminals stay mounted across view switches", async ({
    page,
  }) => {
    await setupAndOpenSession(page);

    await openPicker(page);
    await page.getByTestId("mobile-right-panel-pick-paired").click();
    await page.locator('[data-term="paired"]').waitFor({
      state: "visible",
      timeout: 10_000,
    });

    // Back to the agent: the paired shell is kept alive (hidden), not
    // unmounted, so its PTY and scrollback survive the switch.
    await page.getByTestId("mobile-back-to-agent").click();
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
    await expect(page.locator(".xterm").first()).toBeVisible();
  });
});

test.describe("Desktop right panel split is unchanged (#1452)", () => {
  test.use({ viewport: { width: 1400, height: 900 }, hasTouch: false });

  test("renders the side-by-side split, not the mobile picker", async ({
    page,
  }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await page.locator(".xterm").first().waitFor({ state: "visible", timeout: 10_000 });

    // The desktop split renders the resize handle and never the picker.
    await expect(page.getByTestId("content-split-resize-handle")).toBeVisible();
    await page.getByRole("button", { name: "Toggle diff panel" }).click();
    await expect(page.getByTestId("mobile-right-panel-picker")).toHaveCount(0);
  });
});

async function setupAcpSession(page: Page) {
  await mockTerminalApis(page);
  // Override the session as a running structured view session and stub the structured view
  // panel endpoints; the paired shell still uses the terminal WS, which
  // mockTerminalApis already routes.
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          {
            id: "pinch-test",
            title: "acp-mobile",
            project_path: "/tmp/acp-mobile",
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
          },
        ],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/sessions/*/acp/**", (r) =>
    r.fulfill({ json: {} }),
  );
  await page.goto("/");
  await openMobileSidebar(page);
  await clickSidebarSession(page, "acp-mobile");
  // Structured view sessions render no xterm in the structured view; wait for the
  // right-panel toggle, which only appears once a session is active.
  await page
    .getByRole("button", { name: "Toggle diff panel" })
    .waitFor({ state: "visible", timeout: 10_000 });
}

test.describe("Mobile picker on a structured view session (#1452)", () => {
  test("promotes the paired shell over a structured view session and survives the keyboard", async ({
    page,
  }) => {
    await setupAcpSession(page);
    await openPicker(page);

    await page.getByTestId("mobile-right-panel-pick-paired").click();
    await expect(page.getByTestId("mobile-right-panel-picker")).toHaveCount(0);
    const paired = page.locator('[data-term="paired"]');
    await paired.waitFor({ state: "visible", timeout: 10_000 });

    // The root is pinned for the paired view even on a structured view session, so
    // the terminal stays tall under the keyboard rather than collapsing.
    await simulateKeyboardOpen(page, 300);
    await expect
      .poll(async () => (await paired.boundingBox())?.height ?? 0, {
        message: "paired terminal collapsed on a structured view session",
      })
      .toBeGreaterThan(150);

    // Back to the structured view.
    await page.getByTestId("mobile-back-to-agent").click();
    await expect(page.getByTestId("mobile-back-to-agent")).toHaveCount(0);
  });
});
