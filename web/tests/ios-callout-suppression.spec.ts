// #1451: on mobile Safari a long-press on a session row (an <a href>) raised
// iOS's link-preview sheet and preempted the app's 500ms row menu; the row now
// carries `-webkit-touch-callout: none`. Chromium reports nothing for that
// WebKit-only property, so the assertion is on the class token; the real
// callout behavior is a manual device check.

import { test, expect } from "./helpers/mockedTest";
import { sessionResponse as baseSession } from "./helpers/sessions";
import { mockStaticApis } from "./helpers/apiMocks";
import { devices, type Page } from "@playwright/test";
import { openMobileSidebar } from "./helpers/sidebar";

// iPhone 13 profile: pointer:coarse, hasTouch, mobile viewport.
test.use({ ...devices["iPhone 13"] });

const sessionResponse = () =>
  baseSession({
    id: "s-1",
    title: "demo-ws",
    project_path: "/tmp/repo",
    created_at: "2025-01-01T00:00:00Z",
    branch: "feature/demo",
  });

async function mockApis(page: Page) {
  await mockStaticApis(page);
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [sessionResponse()],
        workspace_ordering: ["/tmp/repo::feature/demo"],
      },
    });
  });
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
