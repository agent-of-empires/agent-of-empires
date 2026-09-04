// Mocked-Playwright coverage for the iOS link-preview callout suppression
// on sidebar session rows (#1451).
//
// On mobile Safari the session row, an <a href>, triggers iOS's native
// link-preview action sheet on long-press, which preempts the app's own
// 500ms rename/delete menu. The fix adds `-webkit-touch-callout: none` via
// the arbitrary Tailwind class `[-webkit-touch-callout:none]` on the row.
//
// `-webkit-touch-callout` is a WebKit-only property, so chromium (the
// engine these mocked specs run on) reports nothing for it via
// getComputedStyle. We therefore assert the class token is present on the
// row: that proves the suppression rule is wired onto the element. The
// real callout-vs-menu behavior is a manual real-device check, noted in
// the issue test plan.

import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { openMobileSidebar } from "./helpers/sidebar";

// iPhone 13 profile: pointer:coarse, hasTouch, mobile viewport.
test.use({ ...devices["iPhone 13"] });

function sessionResponse() {
  return {
    id: "s-1",
    title: "demo-ws",
    project_path: "/tmp/repo",
    group_path: "/tmp/repo",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: "feature/demo",
    main_repo_path: null,
    is_sandboxed: false,
    has_terminal: true,
    profile: "default",
    workspace_repos: [],
  };
}

async function mockApis(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [sessionResponse()],
        workspace_ordering: ["/tmp/repo::feature/demo"],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test.describe("Sidebar iOS callout suppression (#1451)", () => {
  test("session row carries -webkit-touch-callout:none", async ({ page }) => {
    await mockApis(page);
    await page.goto("/");
    await openMobileSidebar(page);

    const row = page.getByTestId("sidebar-session-row").first();
    await expect(row).toBeVisible();
    await expect(row).toHaveClass(/\[-webkit-touch-callout:none\]/);
  });
});
