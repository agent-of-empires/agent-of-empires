import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// User story (#3167): the web Trash section gains a section-level "Empty Trash"
// action mirroring the TUI. It carries the trashed-session count in a
// destructive confirm and purges every trashed workspace by reusing the atomic
// DELETE /api/workspaces endpoint once per workspace. This mocked spec covers
// the two cases named by the issue: (a) Empty Trash purges every trashed
// workspace; (b) with an empty trash it is a no-op (the control is absent).

/** The DELETE /api/workspaces body the web sends: session_ids plus the
 *  DeleteSessionOptions flags. Optional so an assertion can key on the ones a
 *  given test cares about (force_delete, the cleanup flags). */
interface DeleteBody {
  session_ids?: string[];
  force_delete?: boolean;
  delete_worktree?: boolean;
  delete_branch?: boolean;
  delete_sandbox?: boolean;
}

interface Handle {
  /** Session ids the workspace DELETE actually removed, across all calls. */
  deletedIds: string[];
  /** Full body of each DELETE /api/workspaces request, in call order. */
  deleteBodies: DeleteBody[];
}

/** Cleanup opt-in for a trashed session's fixture. `workspaceCleanupDefaults`
 *  gates worktree/branch on `has_cleanable_worktree` and sandbox on
 *  `is_sandboxed`, so both the flag and the matching `cleanup_defaults` entry
 *  must be set for a flag to survive into the DELETE body. */
interface Cleanup {
  cleanableWorktree?: boolean;
  sandboxed?: boolean;
}

function payload(id: string, branch: string, trashed: boolean, cleanup: Cleanup = {}) {
  const cleanable = cleanup.cleanableWorktree ?? false;
  const sandboxed = cleanup.sandboxed ?? false;
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
    is_sandboxed: sandboxed,
    has_cleanable_worktree: cleanable,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    trashed_at: trashed ? new Date().toISOString() : null,
    cleanup_defaults: {
      delete_to_trash: true,
      delete_worktree: cleanable,
      delete_branch: cleanable,
      delete_sandbox: sandboxed,
    },
    workspace_repos: [],
  };
}

async function mockApis(
  page: Page,
  sessions: Array<{ id: string; branch: string; trashed: boolean; cleanup?: Cleanup }>,
  opts: { failDeleteIds?: string[] } = {},
): Promise<Handle> {
  const handle: Handle = { deletedIds: [], deleteBodies: [] };
  const failDeleteIds = new Set(opts.failDeleteIds ?? []);

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    const live = sessions
      .filter((s) => !handle.deletedIds.includes(s.id))
      .map((s) => payload(s.id, s.branch, s.trashed, s.cleanup));
    return r.fulfill({ json: { sessions: live, workspace_ordering: [] } });
  });
  await page.route("**/api/workspaces", (r) => {
    if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
    const body = JSON.parse(r.request().postData() || "{}") as DeleteBody;
    const ids = body.session_ids ?? [];
    handle.deleteBodies.push(body);
    // Partition the workspace's ids: a failed id is reported in a 2xx partial
    // response (populated failed[]) and never added to deletedIds, so the
    // /api/sessions poll keeps returning it and the workspace stays in Trash.
    // A 2xx partial deliberately isolates this feature's summary toast: an
    // all-failed workspace would be a server 500, which additionally trips the
    // pre-existing global fetch-error toast (out of scope here). failDeleteIds
    // defaults empty, so the success-path tests are unchanged.
    const deleted: string[] = [];
    const failed: Array<{ id: string; error: string }> = [];
    for (const id of ids) {
      if (failDeleteIds.has(id)) {
        failed.push({ id, error: "worktree locked" });
      } else {
        handle.deletedIds.push(id);
        deleted.push(id);
      }
    }
    return r.fulfill({ json: { status: failed.length ? "partial" : "deleted", deleted, failed, messages: [] } });
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
      { id: "sess-a", branch: "feat/a", trashed: true, cleanup: { cleanableWorktree: true, sandboxed: true } },
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

    // Every trashed workspace is purged: one atomic DELETE per workspace.
    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);

    // Each DELETE forces removal (force_delete mirrors the TUI, so a dirty
    // worktree cannot block the purge) and carries the workspace's cleanup
    // flags derived by workspaceCleanupDefaults: sess-a opted into worktree and
    // sandbox cleanup, sess-b into neither.
    const byId = new Map(handle.deleteBodies.map((b) => [(b.session_ids ?? [])[0], b]));
    expect(byId.get("sess-a")).toMatchObject({
      session_ids: ["sess-a"],
      force_delete: true,
      delete_worktree: true,
      delete_branch: true,
      delete_sandbox: true,
    });
    expect(byId.get("sess-b")).toMatchObject({
      session_ids: ["sess-b"],
      force_delete: true,
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
    });
    // The Trash control disappears once the trash is empty.
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0, { timeout: 10_000 });
  });

  test("a partial failure keeps the failed workspace in Trash and toasts one summary (#3167)", async ({ page }) => {
    // sess-b's DELETE reports a partial failure, so deleteWorkspaceSessions
    // calls notify.error, which sets anyFailed and drives the single summary
    // error toast. The loop still attempts every workspace (no break on
    // failure), sess-a is purged, and sess-b survives in Trash.
    const handle = await mockApis(
      page,
      [
        { id: "sess-a", branch: "feat/a", trashed: true },
        { id: "sess-b", branch: "feat/b", trashed: true },
      ],
      { failDeleteIds: ["sess-b"] },
    );
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    await expect(page.locator('[data-testid="sidebar-trash-row"]')).toHaveCount(2, { timeout: 10_000 });

    await page.locator('[data-testid="sidebar-trash-empty"]').click();
    const dialog = page.locator('[data-testid="empty-trash-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await dialog.locator('[data-testid="empty-trash-confirm"]').click();

    // The single summary error toast surfaces (Toasts renders role="alert" for
    // errors); the per-workspace toasts are suppressed by handleEmptyTrash.
    await expect(page.getByRole("alert")).toContainText("Some trashed sessions could not be deleted", {
      timeout: 10_000,
    });
    // Both workspaces were attempted (the loop does not break on failure), only
    // sess-a was removed, and sess-b stays in Trash: the Trash control persists
    // (it only renders while something is trashed), unlike the full-purge case
    // where it disappears.
    await expect.poll(() => [...handle.deletedIds], { timeout: 10_000 }).toEqual(["sess-a"]);
    expect(handle.deleteBodies.length).toBe(2);
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(1, { timeout: 10_000 });
  });

  test("is a no-op with an empty trash: the control is absent (#3167)", async ({ page }) => {
    const handle = await mockApis(page, [{ id: "sess-live", branch: "feat/live", trashed: false }]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    // The app is loaded (a live row shows) but with nothing trashed there is no
    // Trash footer control, so Empty Trash is unreachable and no delete fires.
    await expect(page.locator('[data-testid="sidebar-session-row"]').first()).toBeVisible({ timeout: 10_000 });
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0, { timeout: 5_000 });
    await expect(page.locator('[data-testid="sidebar-trash-empty"]')).toHaveCount(0, { timeout: 5_000 });
    expect(handle.deleteBodies.length).toBe(0);
  });
});
