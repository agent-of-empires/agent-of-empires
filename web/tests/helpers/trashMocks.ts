// Session/workspace trash mocks: the sessions list plus the trash, restore and
// workspace-DELETE endpoints the Trash panel drives.

import { expect, type Locator, type Page } from "@playwright/test";
import { mockSessionShellApis, mockStaticApis } from "./apiMocks";
import { sessionResponse } from "./sessions";

export interface TrashSession {
  id: string;
  title?: string;
  trashed: boolean;
  /** Defaults to Stopped when trashed, else Running. */
  status?: string;
  groupPath?: string;
  projectPath?: string;
  branch?: string | null;
  mainRepoPath?: string | null;
  deleteToTrash?: boolean;
  /** A cleanup flag reaches the DELETE only when the session flag and its cleanup_defaults entry are both set. */
  cleanableWorktree?: boolean;
  sandboxed?: boolean;
}

export interface DeleteBody {
  session_ids?: string[];
  force_delete?: boolean;
  delete_worktree?: boolean;
  delete_branch?: boolean;
  delete_sandbox?: boolean;
}

export interface TrashHandle {
  trashedIds: string[];
  restoredIds: string[];
  deletedIds: string[];
  deleteBodies: DeleteBody[];
  failTrash: boolean;
}

export interface TrashMockOptions {
  failDeleteIds?: string[];
  /** Mirror the server's owner-last teardown: reorder ids and abort on the first failure. */
  ownerLast?: boolean;
  /** Slice the sidebar by group_path instead of repo. */
  groupAxis?: boolean;
}

function sessionPayload(s: TrashSession) {
  const cleanable = s.cleanableWorktree ?? false;
  const sandboxed = s.sandboxed ?? false;
  return sessionResponse({
    id: s.id,
    title: s.title,
    project_path: s.projectPath,
    group_path: s.groupPath ?? s.projectPath ?? `/tmp/${s.id}`,
    status: s.status ?? (s.trashed ? "Stopped" : "Running"),
    branch: s.branch ?? null,
    main_repo_path: s.mainRepoPath ?? null,
    is_sandboxed: sandboxed,
    has_cleanable_worktree: cleanable,
    has_managed_worktree: false,
    trashed_at: s.trashed ? new Date().toISOString() : null,
    cleanup_defaults: {
      delete_to_trash: s.deleteToTrash ?? true,
      delete_worktree: cleanable,
      delete_branch: cleanable,
      delete_sandbox: sandboxed,
    },
  });
}

export async function installTrashMocks(
  page: Page,
  sessions: TrashSession[],
  opts: TrashMockOptions = {},
): Promise<TrashHandle> {
  const handle: TrashHandle = {
    trashedIds: [],
    restoredIds: [],
    deletedIds: [],
    deleteBodies: [],
    failTrash: false,
  };
  const state = new Map(sessions.map((s) => [s.id, s]));
  const failDelete = new Set(opts.failDeleteIds ?? []);

  if (opts.groupAxis) {
    await page.addInitScript(() => {
      try {
        localStorage.setItem("aoe-sidebar-axis", "group");
      } catch {
        // Storage may be unavailable; the default axis applies.
      }
    });
    await page.route("**/api/app-state/web-ui-state", (r) => r.fulfill({ json: { "aoe-sidebar-axis": "group" } }));
  }

  await mockStaticApis(page);
  await mockSessionShellApis(page);

  await page.route("**/api/sessions", (r) => {
    if (r.request().method() !== "GET") return r.fulfill({ status: 400 });
    const live = sessions.filter((s) => !handle.deletedIds.includes(s.id)).map((s) => sessionPayload(state.get(s.id)!));
    return r.fulfill({ json: { sessions: live, workspace_ordering: [] } });
  });

  for (const s of sessions) {
    for (const [verb, log, trashed] of [
      ["trash", handle.trashedIds, true],
      ["restore", handle.restoredIds, false],
    ] as const) {
      await page.route(`**/api/sessions/${s.id}/${verb}`, (r) => {
        if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
        log.push(s.id);
        if (trashed && handle.failTrash) return r.fulfill({ status: 500, body: "boom" });
        const next = { ...state.get(s.id)!, trashed };
        state.set(s.id, next);
        return r.fulfill({ json: sessionPayload(next) });
      });
    }
  }

  await page.route("**/api/workspaces", (r) => {
    if (r.request().method() !== "DELETE") return r.fulfill({ status: 400 });
    const body = JSON.parse(r.request().postData() || "{}") as DeleteBody;
    handle.deleteBodies.push(body);
    const ids = body.session_ids ?? [];
    // Owner-last: the opener is deleted after its siblings, and the first
    // failure aborts the rest.
    const ordered = opts.ownerLast && ids.length > 0 ? [...ids.slice(1), ids[0]!] : ids;
    const deleted: string[] = [];
    const failed: Array<{ id: string; error: string }> = [];
    for (const id of ordered) {
      if (failDelete.has(id)) {
        failed.push({ id, error: "worktree locked" });
        if (opts.ownerLast) break;
        continue;
      }
      handle.deletedIds.push(id);
      deleted.push(id);
    }
    // Owner-last teardown answers 500 when nothing was removed; the per-workspace
    // path stays 2xx so a partial keeps its rows without a fetch-error toast.
    if (opts.ownerLast && deleted.length === 0 && failed.length > 0) {
      return r.fulfill({
        status: 500,
        json: { error: "deletion_failed", message: failed.map((f) => f.error).join("; "), failed },
      });
    }
    return r.fulfill({ json: { status: failed.length ? "partial" : "deleted", deleted, failed, messages: [] } });
  });

  return handle;
}

export const sessionRows = (page: Page) => page.locator('[data-testid="sidebar-session-row"]');
export const trashToggle = (page: Page) => page.locator('[data-testid="sidebar-trash-toggle"]');
export const trashRows = (page: Page) => page.locator('[data-testid="sidebar-trash-row"]');
export const deleteDialog = (page: Page) => page.locator('[data-testid="delete-session-dialog"]');

/** Open the Trash panel and wait for its rows. */
export async function openTrash(page: Page, count?: number) {
  await trashToggle(page).click();
  if (count !== undefined) await expect(trashRows(page)).toHaveCount(count, { timeout: 10_000 });
  else await expect(trashRows(page).first()).toBeVisible({ timeout: 10_000 });
}

/** Right-click a sidebar row and pick Delete, landing on the confirm dialog. */
export async function openDeleteDialogFromRow(page: Page, row: Locator) {
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  await page.locator('[data-testid="sidebar-context-menu-delete"]').click();
  const dialog = deleteDialog(page);
  await expect(dialog).toBeVisible({ timeout: 5_000 });
  return dialog;
}

/** Click a trashed row's Delete, landing on the permanent-delete dialog. */
export async function openPurgeDialog(page: Page, row: Locator) {
  await expect(row).toBeVisible({ timeout: 10_000 });
  await row.locator('[data-testid="sidebar-trash-purge"]').click();
  const dialog = deleteDialog(page);
  await expect(dialog).toBeVisible({ timeout: 5_000 });
  return dialog;
}

export const confirmDelete = (dialog: Locator) => dialog.getByRole("button", { name: /^Delete$/ }).click();
