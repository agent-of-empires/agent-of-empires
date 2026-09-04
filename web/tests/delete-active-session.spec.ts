import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// User story (ported from the live Playwright acp-stories suite):
// delete the session you are currently viewing. The row disappears
// from the sidebar and the route falls back to the dashboard.
//
// The confirm dialog's checkbox-to-DELETE-body mapping is covered by
// the DeleteSessionDialog vitest; this spec covers the route + sidebar
// round trip with the DELETE and the post-delete sessions poll stubbed.

interface Handle {
  /** DELETE bodies received for the session, in arrival order. */
  deletes: Array<Record<string, unknown>>;
}

async function mockApis(page: Page): Promise<Handle> {
  const handle: Handle = { deletes: [] };
  const session = {
    id: "sess-active",
    title: "story-delete-active",
    project_path: "/tmp/story",
    group_path: "/tmp",
    tool: "claude",
    status: "Running",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: {},
    workspace_repos: [],
  };

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    // Once the DELETE has landed, the server no longer lists the
    // session; the sidebar poll picks that up and drops the row.
    return r.fulfill({
      json: {
        sessions: handle.deletes.length === 0 ? [session] : [],
        workspace_ordering: [],
      },
    });
  });
  await page.route("**/api/workspaces", (r) => {
    if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
    const body = JSON.parse(r.request().postData() || "{}") as { session_ids?: string[] };
    handle.deletes.push(body);
    return r.fulfill({
      json: { status: "deleted", deleted: body.session_ids ?? [], failed: [], messages: [] },
    });
  });
  await page.route("**/api/sessions/*/ensure", (r) => r.fulfill({ json: { ok: true } }));
  await page.route("**/api/sessions/*/terminal", (r) => r.fulfill({ status: 200, body: "" }));
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({ json: { files: [], per_repo_bases: [], warning: null } }),
  );
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
  await page.routeWebSocket(/\/sessions\/.*\/(ws|acp-ws|container-ws)$/, () => {});
  return handle;
}

test.describe("Delete active session", () => {
  test("deleting the active session removes the row and falls back to /", async ({ page }) => {
    const handle = await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/session/sess-active");
    await expect(page).toHaveURL(/\/session\/sess-active/);

    const row = page.locator('[data-testid="sidebar-session-row"]').filter({ hasText: "story-delete-active" }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();

    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    await expect.poll(() => handle.deletes.length, { timeout: 10_000 }).toBe(1);
    // After deleting the active session the route must leave the
    // /session/:id URL.
    await expect(page).not.toHaveURL(/\/session\/sess-active/, { timeout: 10_000 });
    await expect(row).toHaveCount(0, { timeout: 10_000 });
  });
});
