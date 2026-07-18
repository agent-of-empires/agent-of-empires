import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// "Switch to terminal / structured view" sidebar action (#2252): the
// context-menu item opens a capability-aware confirm dialog, then POSTs the
// acp enable/disable endpoint. The backend round-trip (context preservation,
// tmux teardown) is covered by Rust + live specs; this pins the browser-side
// menu presence, the confirm gate, and the request each direction sends.

interface MockSession {
  id: string;
  title: string;
  view: "structured" | "terminal";
  acp_capable: boolean;
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
          acp_agent: "claude",
          status: "Idle",
          view: s.view,
          acp_capable: s.acp_capable,
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
          smart_rename: "inactive",
          default_name: false,
        })),
        workspace_ordering: [],
      },
    });
  });
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

async function openSwitchMenu(page: Page, title: string) {
  await page.goto("/");
  const row = page.locator("[data-testid='sidebar-session-row']").filter({ hasText: title }).first();
  await row.click({ button: "right" });
  await expect(page.locator("[data-testid='sidebar-context-menu']")).toBeVisible();
  await page.locator("[data-testid='sidebar-context-menu-switch-view']").click();
}

test.describe("Sidebar Switch view (#2252)", () => {
  test("structured session switches to terminal after confirm", async ({ page }) => {
    await mockApis(page, [{ id: "sess-1", title: "Fix login bug", view: "structured", acp_capable: true }]);

    let posted: string | null = null;
    await page.route("**/api/sessions/*/acp/disable", (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted = r.request().url();
      return r.fulfill({ json: { session_id: "sess-1", view: "terminal" } });
    });

    await openSwitchMenu(page, "Fix login bug");
    // Claude keeps context: the confirm copy must say so, not threaten loss.
    const dialog = page.locator("[data-testid='switch-view-dialog']");
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText("continues in the terminal");
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect.poll(() => posted).toContain("/api/sessions/sess-1/acp/disable");
    await expect(page.getByText("Switched to terminal")).toBeVisible();
  });

  test("a failed switch surfaces an error toast", async ({ page }) => {
    await mockApis(page, [{ id: "sess-9", title: "Broken switch", view: "structured", acp_capable: true }]);
    await page.route("**/api/sessions/*/acp/disable", (r) => r.fulfill({ status: 500 }));

    await openSwitchMenu(page, "Broken switch");
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect(page.getByText("Failed to switch to terminal")).toBeVisible();
  });

  test("terminal acp-capable session switches to structured after confirm", async ({ page }) => {
    await mockApis(page, [{ id: "sess-2", title: "Terminal claude", view: "terminal", acp_capable: true }]);

    let posted: string | null = null;
    await page.route("**/api/sessions/*/acp/enable", (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      posted = r.request().url();
      return r.fulfill({ json: { session_id: "sess-2", view: "structured" } });
    });

    await openSwitchMenu(page, "Terminal claude");
    await expect(page.locator("[data-testid='switch-view-dialog']")).toBeVisible();
    await page.locator("[data-testid='switch-view-confirm']").click();
    await expect.poll(() => posted).toContain("/api/sessions/sess-2/acp/enable");
  });

  test("non-acp-capable terminal session has no switch-view item", async ({ page }) => {
    await mockApis(page, [{ id: "sess-3", title: "Plain terminal", view: "terminal", acp_capable: false }]);
    await page.goto("/");
    const row = page.locator("[data-testid='sidebar-session-row']").filter({ hasText: "Plain terminal" }).first();
    await row.click({ button: "right" });
    await expect(page.locator("[data-testid='sidebar-context-menu']")).toBeVisible();
    await expect(page.locator("[data-testid='sidebar-context-menu-switch-view']")).toHaveCount(0);
  });
});
