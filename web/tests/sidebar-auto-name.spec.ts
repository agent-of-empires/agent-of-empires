import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// "Auto-name now" sidebar recovery (#2347): the context-menu item re-triggers
// smart rename for a structured session whose automatic rename never landed.
// It is shown only while the session is still default-named (server-provided
// `default_name`), so it never overwrites a chosen title, and POSTs the
// smart-rename endpoint. The backend round-trip (clears the attempted gate,
// 409 on a named session) is covered by Rust tests; this pins the browser-side
// menu gating and request.

interface MockSession {
  id: string;
  title: string;
  default_name: boolean;
}

async function mockApis(page: Page, sessions: MockSession[]) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    return r.fulfill({
      json: {
        sessions: sessions.map((s) => ({
          id: s.id,
          title: s.title,
          project_path: "/tmp/repo",
          group_path: "/tmp/repo",
          tool: "claude",
          status: "Idle",
          view: "structured",
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
          smart_rename: s.default_name ? "pending" : "inactive",
          default_name: s.default_name,
        })),
        workspace_ordering: [],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

test.describe("Sidebar Auto-name now (#2347)", () => {
  test("re-triggers smart rename for a still-default-named session", async ({ page }) => {
    await mockApis(page, [{ id: "sess-default", title: "Vikings", default_name: true }]);

    let posted: string | null = null;
    await page.route("**/api/sessions/*/smart-rename", (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted = r.request().url();
      return r.fulfill({ status: 202 });
    });

    await page.goto("/");
    const row = page.locator("[data-testid='sidebar-session-row']").filter({ hasText: "Vikings" }).first();
    await row.click({ button: "right" });
    await expect(page.locator("[data-testid='sidebar-context-menu']")).toBeVisible();

    await page.locator("[data-testid='sidebar-context-menu-auto-name']").click();
    await expect.poll(() => posted).toContain("/api/sessions/sess-default/smart-rename");
  });

  test("hides the action for an already-named session", async ({ page }) => {
    await mockApis(page, [{ id: "sess-named", title: "Fix login bug", default_name: false }]);
    await page.goto("/");

    const row = page.locator("[data-testid='sidebar-session-row']").filter({ hasText: "Fix login bug" }).first();
    await row.click({ button: "right" });
    await expect(page.locator("[data-testid='sidebar-context-menu']")).toBeVisible();

    // The menu opened (Switch agent is present for a structured session) but
    // Auto-name now is absent because the session already has a custom name.
    await expect(page.locator("[data-testid='sidebar-context-menu-switch-agent']")).toBeVisible();
    await expect(page.locator("[data-testid='sidebar-context-menu-auto-name']")).toHaveCount(0);
  });
});
