// A mocked structured-view session driven straight off the REST stubs, for the
// specs that push their own frames instead of using acpMock's replay socket.

import { expect, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./sidebar";
import { sessionResponse } from "./sessions";

const BOOT_PATHS = [
  "settings",
  "themes",
  "agents",
  "profiles",
  "groups",
  "devices",
  "docker/status",
  "about",
  "system/update-status",
];
const OBJECT_PATHS = new Set(["settings", "docker/status", "about", "system/update-status"]);

/** Boot stubs plus one running structured-view session. Sockets are accepted
 *  but silent, so the session reads as connected; a spec that needs frames
 *  re-routes them afterwards (later registrations win). */
export async function mockStructuredSessionApis(page: Page, opts: { id: string; title: string; projectPath?: string }) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  for (const path of BOOT_PATHS) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: OBJECT_PATHS.has(path) ? {} : [] }));
  }
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() === "POST") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: [
          sessionResponse({
            id: opts.id,
            title: opts.title,
            project_path: opts.projectPath ?? `/tmp/${opts.title}`,
            group_path: "/tmp",
            status: "Running",
            view: "structured",
            acp_worker_state: "running",
            claude_fullscreen: false,
          }),
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

/** Open the session from the mobile sidebar and wait for the structured view. */
export async function openStructuredViewFor(page: Page, title: string) {
  await page.goto("/");
  await expect(page.locator("header")).toBeVisible();
  // On a mobile viewport the sidebar is collapsed behind a toggle.
  await openMobileSidebar(page);
  await clickSidebarSession(page, title);
  await expect(page.getByTestId("structured-view-root")).toBeVisible({ timeout: 10_000 });
}
