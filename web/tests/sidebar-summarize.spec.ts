import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// "Summarize conversation" sidebar action (#2808): the context-menu item
// requests an on-demand conversation summary for a structured session by
// POSTing the summarize endpoint. Unlike Auto-name now, it is offered for any
// structured session (named or not), since it does not touch the title. The
// backend round-trip (eligibility gate, one-shot, ConversationSummary event)
// is covered by Rust tests; this pins the browser-side menu presence and the
// request.

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

test.describe("Sidebar Summarize conversation (#2808)", () => {
  test("requests an on-demand summary for a structured session", async ({ page }) => {
    // A custom-named session: the action is still offered (unlike Auto-name).
    await mockApis(page, [{ id: "sess-1", title: "Fix login bug", default_name: false }]);

    let posted: string | null = null;
    await page.route("**/api/sessions/*/summarize", (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted = r.request().url();
      return r.fulfill({ status: 202 });
    });

    await page.goto("/");
    const row = page.locator("[data-testid='sidebar-session-row']").filter({ hasText: "Fix login bug" }).first();
    await row.click({ button: "right" });
    await expect(page.locator("[data-testid='sidebar-context-menu']")).toBeVisible();

    await page.locator("[data-testid='sidebar-context-menu-summarize']").click();
    await expect.poll(() => posted).toContain("/api/sessions/sess-1/summarize");
  });
});
