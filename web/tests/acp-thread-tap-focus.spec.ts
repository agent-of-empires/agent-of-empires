import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";

// Tapping anywhere in the structured-view transcript focuses the composer and
// brings up the soft keyboard on touch (#2243). On a coarse pointer the
// composer is NOT auto-focused on mount (#1178), so a tap on the transcript is
// the only thing that should move focus into it here.

test.use({ ...devices["iPhone 13"] });

const SESSION_ID = "sess-acp-tap";
const TITLE = "acp-tap";

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
            project_path: "/tmp/acp-tap",
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
  await openMobileSidebar(page);
  await clickSidebarSession(page, TITLE);
  await expect(page.getByTestId("structured-view-root")).toBeVisible({ timeout: 10000 });
}

test.describe("Structured-view tap-to-focus (#2243)", () => {
  test("tapping the transcript focuses the composer", async ({ page }) => {
    await setup(page);
    await openStructuredSession(page);

    const composer = page.getByPlaceholder(/Send a message/);
    await expect(composer).toBeVisible();
    // Start from an unfocused composer so the tap is what moves focus into it.
    await composer.blur();
    await expect(composer).not.toBeFocused();

    // Tap an empty area near the top of the transcript (away from the centered
    // starter-prompt buttons) so the tap lands on non-interactive content.
    await page.getByTestId("acp-viewport").click({ position: { x: 8, y: 8 } });

    await expect(composer).toBeFocused();
  });
});
