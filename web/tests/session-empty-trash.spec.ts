import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// User story (#3167): the web Trash section gains a section-level "Empty Trash"
// action mirroring the TUI. It carries the trashed-session count in a
// destructive confirm and purges every trashed workspace by reusing the atomic
// DELETE /api/workspaces endpoint once per workspace. This mocked spec covers
// the two cases named by the issue: (a) Empty Trash purges every trashed
// workspace; (b) with an empty trash it is a no-op (the control is absent).

interface Handle {
  /** Session ids the workspace DELETE actually removed, across all calls. */
  deletedIds: string[];
  /** session_ids arrays of each DELETE /api/workspaces request, in call order. */
  deleteBodies: string[][];
}

function payload(id: string, branch: string, trashed: boolean) {
  return {
    id,
    title: id,
    project_path: `/tmp/${id}`,
    group_path: `/tmp/${id}`,
    tool: "claude",
    status: trashed ? "Stopped" : "Running",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch,
    main_repo_path: `/tmp/${id}`,
    is_sandboxed: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    trashed_at: trashed ? new Date().toISOString() : null,
    cleanup_defaults: { delete_to_trash: true },
    workspace_repos: [],
  };
}

async function mockApis(
  page: Page,
  sessions: Array<{ id: string; branch: string; trashed: boolean }>,
): Promise<Handle> {
  const handle: Handle = { deletedIds: [], deleteBodies: [] };

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    const live = sessions
      .filter((s) => !handle.deletedIds.includes(s.id))
      .map((s) => payload(s.id, s.branch, s.trashed));
    return r.fulfill({ json: { sessions: live, workspace_ordering: [] } });
  });
  await page.route("**/api/workspaces", (r) => {
    if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
    const body = JSON.parse(r.request().postData() || "{}") as { session_ids?: string[] };
    const ids = body.session_ids ?? [];
    handle.deleteBodies.push(ids);
    for (const id of ids) handle.deletedIds.push(id);
    return r.fulfill({ json: { status: "deleted", deleted: ids, failed: [], messages: [] } });
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

test.describe("Empty Trash", () => {
  test("purges every trashed workspace after confirm (#3167)", async ({ page }) => {
    const handle = await mockApis(page, [
      { id: "sess-a", branch: "feat/a", trashed: true },
      { id: "sess-b", branch: "feat/b", trashed: true },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    await expect(page.locator('[data-testid="sidebar-trash-row"]')).toHaveCount(2, { timeout: 10_000 });

    await page.locator('[data-testid="sidebar-trash-empty"]').click();
    const dialog = page.locator('[data-testid="empty-trash-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    // The confirm carries the trashed-session count, mirroring the TUI wording.
    await expect(dialog).toContainText("Permanently delete 2 trashed sessions? This cannot be undone.");

    await dialog.locator('[data-testid="empty-trash-confirm"]').click();

    // Every trashed workspace is purged: one atomic DELETE per workspace, and
    // the two single-session workspaces cover both trashed sessions.
    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
    expect(handle.deleteBodies.length).toBe(2);
    // The Trash control disappears once the trash is empty.
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0, { timeout: 10_000 });
  });

  test("is a no-op with an empty trash: the control is absent (#3167)", async ({ page }) => {
    const handle = await mockApis(page, [{ id: "sess-live", branch: "feat/live", trashed: false }]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    // The app is loaded (a live row shows) but with nothing trashed there is no
    // Trash footer control, so Empty Trash is unreachable and no delete fires.
    await expect(page.locator('[data-testid="sidebar-session-row"]').first()).toBeVisible({ timeout: 10_000 });
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0);
    await expect(page.locator('[data-testid="sidebar-trash-empty"]')).toHaveCount(0);
    expect(handle.deleteBodies.length).toBe(0);
  });
});
