import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

// User story (#2489): trash-first delete. Right-clicking a session and
// confirming the default (non-permanent) Delete moves it to the sidebar
// Trash section instead of destroying it; Restore from that section brings
// it back to the active list.
//
// The dialog's checkbox-to-body mapping is covered by the DeleteSessionDialog
// vitest and the live session-trash spec covers the backend round trip; this
// mocked spec deterministically exercises the App trash/restore handlers and
// the WorkspaceSidebar Trash panel render + actions for coverage.

interface Handle {
  trashed: boolean;
  trashCalls: number;
  restoreCalls: number;
  deletes: number;
  failTrash: boolean;
  failRestore: boolean;
}

function sessionPayload(trashed: boolean, title = "story-trash") {
  return {
    id: "sess-trash",
    title,
    project_path: "/tmp/story",
    group_path: "/tmp",
    tool: "claude",
    status: trashed ? "Stopped" : "Running",
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
    trashed_at: trashed ? new Date().toISOString() : null,
    cleanup_defaults: { delete_to_trash: true },
    workspace_repos: [],
  };
}

async function mockApis(page: Page, options: { title?: string } = {}): Promise<Handle> {
  const handle: Handle = {
    trashed: false,
    trashCalls: 0,
    restoreCalls: 0,
    deletes: 0,
    failTrash: false,
    failRestore: false,
  };

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    const sessions = handle.deletes > 0 ? [] : [sessionPayload(handle.trashed, options.title)];
    return r.fulfill({ json: { sessions, workspace_ordering: [] } });
  });
  await page.route("**/api/sessions/sess-trash/trash", (r) => {
    if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
    handle.trashCalls += 1;
    if (handle.failTrash) return r.fulfill({ status: 500, body: "boom" });
    handle.trashed = true;
    return r.fulfill({ json: sessionPayload(true, options.title) });
  });
  await page.route("**/api/sessions/sess-trash/restore", (r) => {
    if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
    handle.restoreCalls += 1;
    if (handle.failRestore) return r.fulfill({ status: 500, body: "boom" });
    handle.trashed = false;
    return r.fulfill({ json: sessionPayload(false, options.title) });
  });
  await page.route("**/api/sessions/sess-trash", (r) => {
    if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
    handle.deletes += 1;
    return r.fulfill({ json: {} });
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

test.describe("Session trash flow", () => {
  test("trash moves the row into Trash, restore brings it back", async ({ page }) => {
    const handle = await mockApis(page);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/session/sess-trash");
    const row = page.locator('[data-testid="sidebar-session-row"]').filter({ hasText: "story-trash" }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    // Default (non-permanent) Delete -> trash path.
    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).not.toBeChecked();
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    await expect.poll(() => handle.trashCalls, { timeout: 10_000 }).toBe(1);

    // Row leaves the active list; the footer Trash control appears and its
    // panel lists the trashed workspace.
    const trashToggle = page.locator('[data-testid="sidebar-trash-toggle"]');
    await expect(trashToggle).toBeVisible({ timeout: 10_000 });
    await expect(trashToggle).toContainText("Trash");
    await trashToggle.click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').filter({ hasText: "story-trash" });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });
    await expect(trashRow.locator('[data-testid="sidebar-trash-open"]')).toContainText("Open");
    await expect(trashRow.locator('[data-testid="sidebar-trash-restore"]')).toContainText("Restore");
    await expect(trashRow.locator('[data-testid="sidebar-trash-purge"]')).toContainText("Delete");

    // Restore brings it back to the active list.
    await trashRow.locator('[data-testid="sidebar-trash-restore"]').click();
    await expect.poll(() => handle.restoreCalls, { timeout: 10_000 }).toBe(1);
    await expect(trashToggle).toHaveCount(0, { timeout: 10_000 });
    await expect(row).toBeVisible({ timeout: 10_000 });
  });

  test("a failed trash surfaces an error and keeps the row", async ({ page }) => {
    const handle = await mockApis(page);
    handle.failTrash = true;
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/session/sess-trash");
    const row = page.locator('[data-testid="sidebar-session-row"]').filter({ hasText: "story-trash" }).first();
    await expect(row).toBeVisible({ timeout: 10_000 });

    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();
    await page
      .locator('[data-testid="delete-session-dialog"]')
      .getByRole("button", { name: /^Delete$/ })
      .click();

    await expect.poll(() => handle.trashCalls, { timeout: 10_000 }).toBe(1);
    // The trash failed: no Trash icon appears and the row stays put.
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0, { timeout: 5_000 });
  });

  test("Delete from the Trash panel opens the permanent-delete dialog", async ({ page }) => {
    const handle = await mockApis(page);
    handle.trashed = true; // start already trashed
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').filter({ hasText: "story-trash" });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });

    // The Trash panel Delete re-opens the dialog; with the row already
    // trashed it goes straight to permanent delete (no trash checkbox).
    await trashRow.locator('[data-testid="sidebar-trash-purge"]').click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).toHaveCount(0);
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    await expect.poll(() => handle.deletes, { timeout: 10_000 }).toBe(1);
  });

  test("long trashed session names keep Trash actions and the delete dialog usable", async ({ page }) => {
    const longTitle = `story-trash-${"x".repeat(240)}`;
    const handle = await mockApis(page, { title: longTitle });
    handle.trashed = true;
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    const trashToggle = page.locator('[data-testid="sidebar-trash-toggle"]');
    await trashToggle.click();
    const panelBox = await page.locator('[data-testid="sidebar-trash-menu"]').boundingBox();
    const toggleBox = await trashToggle.boundingBox();
    expect(panelBox).not.toBeNull();
    expect(toggleBox).not.toBeNull();
    expect(panelBox!.x + panelBox!.width).toBeGreaterThan(toggleBox!.x + toggleBox!.width + 100);
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').filter({ hasText: longTitle });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });

    await expect(trashRow.locator('[data-testid="sidebar-trash-open"]')).toContainText("Open");
    await expect(trashRow.locator('[data-testid="sidebar-trash-restore"]')).toContainText("Restore");
    await expect(trashRow.locator('[data-testid="sidebar-trash-restore"]')).toBeInViewport({ ratio: 1 });
    const purge = trashRow.locator('[data-testid="sidebar-trash-purge"]');
    await expect(purge).toContainText("Delete");
    await expect(purge).toBeInViewport({ ratio: 1 });

    await purge.click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog).toContainText(longTitle);
    await expect(dialog.getByRole("button", { name: /^Delete$/ })).toBeInViewport({ ratio: 1 });
    const panelFits = await dialog.locator('[data-testid="delete-session-dialog-panel"]').evaluate((node) => {
      return node.scrollWidth <= node.clientWidth;
    });
    expect(panelFits).toBe(true);

    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    await expect.poll(() => handle.deletes, { timeout: 10_000 }).toBe(1);
  });
});

// Multi-session workspace coverage (#2530, #2533). A "workspace" is keyed by
// `repoPath::branch`, so two sessions on the same branch but different
// `group_path` belong to ONE workspace that the sidebar splits into two
// per-group slices. Trash membership, Restore, and permanent Delete must all
// act on the whole workspace, not on whichever slice survives dedupe.

interface MultiHandle {
  /** Session ids that received a trash POST, in call order. */
  trashedIds: string[];
  /** Session ids that received a DELETE, in call order. */
  deletedIds: string[];
  /** Last DELETE option body keyed by session id. */
  deleteOptions: Record<string, Record<string, unknown>>;
  /** Session ids that received a restore POST, in call order. */
  restoredIds: string[];
}

function multiPayload(id: string, groupPath: string, trashed: boolean, deleteToTrash = true) {
  return {
    id,
    title: id,
    project_path: "/tmp/repo",
    group_path: groupPath,
    tool: "claude",
    status: trashed ? "Stopped" : "Running",
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: "feat/x",
    main_repo_path: "/tmp/repo",
    is_sandboxed: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    trashed_at: trashed ? new Date().toISOString() : null,
    cleanup_defaults: { delete_to_trash: deleteToTrash },
    workspace_repos: [],
  };
}

async function mockMultiApis(
  page: Page,
  sessions: Array<{ id: string; groupPath: string; trashed: boolean; deleteToTrash?: boolean }>,
  opts: { failDeleteIds?: string[] } = {},
): Promise<MultiHandle> {
  const handle: MultiHandle = { trashedIds: [], deletedIds: [], deleteOptions: {}, restoredIds: [] };
  const trashedState = new Map(sessions.map((s) => [s.id, s.trashed]));
  const failDelete = new Set(opts.failDeleteIds ?? []);

  // Force the user-group axis so `buildSessionGroups` actually slices the
  // workspace by `group_path`. The slicing bug (#2533) cannot reproduce on the
  // default repo axis, where rows already carry the full unsliced workspace.
  await page.addInitScript(() => {
    try {
      localStorage.setItem("aoe-sidebar-axis", "group");
    } catch {
      // jsdom-less / storage-disabled contexts fall back to the default axis.
    }
  });
  await page.route("**/api/app-state/web-ui-state", (r) => r.fulfill({ json: { "aoe-sidebar-axis": "group" } }));

  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    const live = sessions
      .filter((s) => !handle.deletedIds.includes(s.id))
      .map((s) => multiPayload(s.id, s.groupPath, trashedState.get(s.id) ?? false, s.deleteToTrash));
    return r.fulfill({ json: { sessions: live, workspace_ordering: [] } });
  });
  for (const s of sessions) {
    await page.route(`**/api/sessions/${s.id}/trash`, (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      handle.trashedIds.push(s.id);
      trashedState.set(s.id, true);
      return r.fulfill({ json: multiPayload(s.id, s.groupPath, true, s.deleteToTrash) });
    });
    await page.route(`**/api/sessions/${s.id}/restore`, (r) => {
      if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
      handle.restoredIds.push(s.id);
      trashedState.set(s.id, false);
      return r.fulfill({ json: multiPayload(s.id, s.groupPath, false, s.deleteToTrash) });
    });
    await page.route(`**/api/sessions/${s.id}`, (r) => {
      if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
      if (failDelete.has(s.id)) return r.fulfill({ status: 500, json: { message: "boom" } });
      handle.deletedIds.push(s.id);
      const body = r.request().postData();
      handle.deleteOptions[s.id] = body ? (JSON.parse(body) as Record<string, unknown>) : {};
      return r.fulfill({ json: {} });
    });
  }
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

test.describe("Multi-session workspace trash", () => {
  test("live workspace Delete presents workspace trash scope (#2538)", async ({ page }) => {
    const handle = await mockMultiApis(page, [
      { id: "sess-a", groupPath: "alpha", trashed: false },
      { id: "sess-b", groupPath: "alpha", trashed: false },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    const row = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();

    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog.locator("#delete-session-dialog-title")).toContainText("Delete Workspace");
    await expect(dialog).toContainText("Move this workspace to Trash?");
    await expect(dialog.locator('[data-testid="delete-session-affected-count"]')).toContainText("all 2 sessions");
    await expect(dialog.locator('[data-testid="delete-session-affected-list"]')).toContainText("sess-a");
    await expect(dialog.locator('[data-testid="delete-session-affected-list"]')).toContainText("sess-b");

    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    await expect.poll(() => [...handle.trashedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
  });

  test("workspace Delete uses trash-first when any live session defaults to Trash (#2538)", async ({ page }) => {
    await mockMultiApis(page, [
      { id: "sess-a", groupPath: "alpha", trashed: false, deleteToTrash: false },
      { id: "sess-b", groupPath: "alpha", trashed: false, deleteToTrash: true },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    const row = page.locator('[data-testid="sidebar-session-row"]').first();
    await expect(row).toBeVisible({ timeout: 10_000 });
    await row.click({ button: "right" });
    await page.locator('[data-testid="sidebar-context-menu-delete"]').click();

    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toContainText("Move this workspace to Trash?", { timeout: 5_000 });
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).toBeVisible();
  });

  test("a workspace trashed in only one group slice does not appear in Trash (#2533)", async ({ page }) => {
    await mockMultiApis(page, [
      { id: "sess-a", groupPath: "alpha", trashed: true },
      { id: "sess-b", groupPath: "beta", trashed: false },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    // The still-live sibling keeps the workspace out of Trash entirely: no
    // Trash footer icon, even though the "alpha" slice is fully trashed.
    await expect(page.locator('[data-testid="sidebar-session-row"]').first()).toBeVisible({ timeout: 10_000 });
    await expect(page.locator('[data-testid="sidebar-trash-toggle"]')).toHaveCount(0, { timeout: 5_000 });
  });

  test("Restore from Trash restores every session of a split workspace (#2533)", async ({ page }) => {
    const handle = await mockMultiApis(page, [
      { id: "sess-a", groupPath: "alpha", trashed: true },
      { id: "sess-b", groupPath: "beta", trashed: true },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    // The two slices dedupe to a single Trash row for the workspace.
    const trashRows = page.locator('[data-testid="sidebar-trash-row"]');
    await expect(trashRows).toHaveCount(1, { timeout: 10_000 });

    await trashRows.first().locator('[data-testid="sidebar-trash-restore"]').click();
    await expect.poll(() => [...handle.restoredIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
  });

  test("permanent Delete purges every session of a trashed workspace (#2530)", async ({ page }) => {
    const handle = await mockMultiApis(page, [
      { id: "sess-a", groupPath: "alpha", trashed: true },
      { id: "sess-b", groupPath: "beta", trashed: true },
    ]);
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').first();
    await expect(trashRow).toBeVisible({ timeout: 10_000 });

    await trashRow.locator('[data-testid="sidebar-trash-purge"]').click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    // The dialog presents the whole workspace scope (#2530, #2538).
    await expect(dialog.locator("#delete-session-dialog-title")).toContainText("Delete Workspace");
    await expect(dialog).toContainText("Permanently delete this workspace?");
    await expect(dialog.locator('[data-testid="delete-session-affected-count"]')).toContainText("all 2 sessions");
    await expect(dialog.locator('[data-testid="delete-session-affected-list"]')).toContainText("sess-a");
    await expect(dialog.locator('[data-testid="delete-session-affected-list"]')).toContainText("sess-b");
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
    // Whichever id is the workspace primary, the sibling never re-runs the
    // shared worktree/branch removal.
    const siblingId = handle.deletedIds[1]!;
    expect(handle.deleteOptions[siblingId]).toMatchObject({ delete_worktree: false, delete_branch: false });
  });

  test("a sibling delete failure leaves that session in Trash and reports it (#2530)", async ({ page }) => {
    // Primary (sess-a) and one sibling (sess-b) purge, but sess-c fails. The
    // workspace stays in Trash holding the surviving session, surfacing the
    // partial-failure path of the delete loop.
    const handle = await mockMultiApis(
      page,
      [
        { id: "sess-a", groupPath: "alpha", trashed: true },
        { id: "sess-b", groupPath: "beta", trashed: true },
        { id: "sess-c", groupPath: "gamma", trashed: true },
      ],
      { failDeleteIds: ["sess-c"] },
    );
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    const toggle = page.locator('[data-testid="sidebar-trash-toggle"]');
    await toggle.click();
    await page.locator('[data-testid="sidebar-trash-purge"]').first().click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    // The two that succeeded are gone; the failed one is still attempted.
    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
    // The workspace remains in Trash because sess-c is still trashed.
    await expect(toggle).toBeVisible({ timeout: 10_000 });
  });

  test("a failed primary delete does not redirect away from an open sibling (#2539 review)", async ({ page }) => {
    // Open session is the sibling (sess-b); the primary (sess-a) delete fails,
    // so nothing is removed. The user must stay on the still-live session
    // instead of being kicked back to "/".
    const handle = await mockMultiApis(
      page,
      [
        { id: "sess-a", groupPath: "alpha", trashed: true },
        { id: "sess-b", groupPath: "beta", trashed: true },
      ],
      { failDeleteIds: ["sess-a"] },
    );
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/session/sess-b");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').first();
    await expect(trashRow).toBeVisible({ timeout: 10_000 });
    await trashRow.locator('[data-testid="sidebar-trash-purge"]').click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    // Dialog closes (the handler ran), the primary delete failed so no session
    // was removed, and the route still points at the open sibling.
    await expect(dialog).toHaveCount(0, { timeout: 10_000 });
    await expect.poll(() => handle.deletedIds, { timeout: 5_000 }).toEqual([]);
    await expect(page).toHaveURL(/\/session\/sess-b/);
  });

  for (const open of ["sess-a", "sess-b"] as const) {
    test(`redirects to / after the open ${open === "sess-a" ? "primary" : "sibling"} is purged`, async ({ page }) => {
      await mockMultiApis(page, [
        { id: "sess-a", groupPath: "alpha", trashed: true },
        { id: "sess-b", groupPath: "beta", trashed: true },
      ]);
      await page.setViewportSize({ width: 1280, height: 720 });

      await page.goto(`/session/${open}`);
      await page.locator('[data-testid="sidebar-trash-toggle"]').click();
      const trashRow = page.locator('[data-testid="sidebar-trash-row"]').first();
      await expect(trashRow).toBeVisible({ timeout: 10_000 });
      await trashRow.locator('[data-testid="sidebar-trash-purge"]').click();
      const dialog = page.locator('[data-testid="delete-session-dialog"]');
      await expect(dialog).toBeVisible({ timeout: 5_000 });
      await dialog.getByRole("button", { name: /^Delete$/ }).click();

      // The open session was deleted, so the handler navigates home.
      await expect(page).toHaveURL(/\/$/, { timeout: 10_000 });
    });
  }
});
