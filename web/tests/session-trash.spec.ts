import { test, expect } from "./helpers/mockedTest";
import type { Locator, Page } from "@playwright/test";
import {
  confirmDelete,
  installTrashMocks,
  openDeleteDialogFromRow,
  openPurgeDialog,
  openTrash,
  sessionRows,
  trashRows,
  trashToggle,
} from "./helpers/trashMocks";

test.use({ viewport: { width: 1280, height: 720 } });

// #2489: trash and restore round-trip against the server in live/session-actions.spec.ts.
test.describe("Session trash flow", () => {
  const SESSION = { id: "sess-trash", title: "story-trash", projectPath: "/tmp/story", groupPath: "/tmp" };

  test("Delete on a long-named trashed row opens a usable permanent-delete dialog", async ({ page }) => {
    const title = `story-trash-${"x".repeat(240)}`;
    const handle = await installTrashMocks(page, [{ ...SESSION, title, trashed: true }]);
    await page.goto("/");

    await trashToggle(page).click();
    const panelBox = (await page.locator('[data-testid="sidebar-trash-menu"]').boundingBox())!;
    const toggleBox = (await trashToggle(page).boundingBox())!;
    expect(panelBox.x + panelBox.width).toBeGreaterThan(toggleBox.x + toggleBox.width + 100);

    const trashRow = trashRows(page).filter({ hasText: title });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });
    await expect(trashRow.locator('[data-testid="sidebar-trash-open"]')).toContainText("Open");
    const restore = trashRow.locator('[data-testid="sidebar-trash-restore"]');
    await expect(restore).toContainText("Restore");
    await expect(restore).toBeInViewport({ ratio: 1 });
    const purge = trashRow.locator('[data-testid="sidebar-trash-purge"]');
    await expect(purge).toContainText("Delete");
    await expect(purge).toBeInViewport({ ratio: 1 });

    // Deleting an already-trashed row goes straight to permanent delete.
    const dialog = await openPurgeDialog(page, trashRow);
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).toHaveCount(0);
    await expect(dialog).toContainText(title);
    await expect(dialog.getByRole("button", { name: /^Delete$/ })).toBeInViewport({ ratio: 1 });
    const panelFits = await dialog
      .locator('[data-testid="delete-session-dialog-panel"]')
      .evaluate((node) => node.scrollWidth <= node.clientWidth);
    expect(panelFits).toBe(true);

    await confirmDelete(dialog);
    await expect.poll(() => handle.deletedIds.length, { timeout: 10_000 }).toBe(1);
  });
});

// #2530, #2533: sessions on one `repoPath::branch` form one workspace even when
// split across user groups; trash, restore and delete act on the whole workspace.
test.describe("Multi-session workspace trash", () => {
  // The group axis slices the workspace by group_path; the repo axis cannot reproduce #2533.
  const workspace = (
    ...parts: Array<{
      id: string;
      groupPath: string;
      trashed: boolean;
      deleteToTrash?: boolean;
      status?: string;
      cleanableWorktree?: boolean;
    }>
  ) => parts.map((p) => ({ ...p, projectPath: "/tmp/repo", mainRepoPath: "/tmp/repo", branch: "feat/x" }));
  const install = (page: Page, sessions: ReturnType<typeof workspace>, failDeleteIds?: string[]) =>
    installTrashMocks(page, sessions, { groupAxis: true, ownerLast: true, failDeleteIds });

  const affected = (dialog: Locator) => ({
    count: dialog.locator('[data-testid="delete-session-affected-count"]'),
    list: dialog.locator('[data-testid="delete-session-affected-list"]'),
  });

  // #2538: the dialog is workspace-shaped and trash-first when any live session defaults to Trash.
  test("live workspace Delete presents workspace trash scope, trash-first if any session defaults to Trash (#2538)", async ({
    page,
  }) => {
    const handle = await install(
      page,
      workspace(
        { id: "sess-a", groupPath: "alpha", trashed: false, deleteToTrash: false },
        { id: "sess-b", groupPath: "alpha", trashed: false, deleteToTrash: true },
      ),
    );
    await page.goto("/");

    const dialog = await openDeleteDialogFromRow(page, sessionRows(page).first());
    await expect(dialog.locator("#delete-session-dialog-title")).toContainText("Delete Workspace");
    await expect(dialog).toContainText("Move this workspace to Trash?");
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).toBeVisible();
    await expect(affected(dialog).count).toContainText("all 2 sessions");
    await expect(affected(dialog).list).toContainText("sess-a");
    await expect(affected(dialog).list).toContainText("sess-b");

    await confirmDelete(dialog);
    await expect.poll(() => [...handle.trashedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
  });

  test("stop, start, and delete on a group slice act only on that row's sessions (#4019)", async ({ page }) => {
    const handle = await install(
      page,
      workspace(
        { id: "sess-a", groupPath: "alpha", trashed: false, status: "Stopped" },
        { id: "sess-b", groupPath: "beta", trashed: false },
        { id: "sess-c", groupPath: "gamma", trashed: false, status: "Stopped" },
        { id: "sess-d", groupPath: "gamma", trashed: false, status: "Stopped" },
      ),
    );
    const lifecycle: string[] = [];
    await page.route(/\/api\/sessions\/[^/]+\/(stop|start)$/, (r) => {
      const [, id, verb] = new URL(r.request().url()).pathname.match(/sessions\/([^/]+)\/(\w+)$/)!;
      lifecycle.push(`${verb} ${id}`);
      return r.fulfill({ json: { id } });
    });
    await page.goto("/");
    // A two-session row is labelled by its branch, so gamma is the row naming neither single session.
    const beta = sessionRows(page).filter({ hasText: "sess-b" });
    const gamma = sessionRows(page).filter({ hasNotText: /sess-[ab]/ });
    const menu = async (row: Locator, item: string) => {
      await expect(row).toBeVisible({ timeout: 10_000 });
      await row.click({ button: "right" });
      await page.locator(`[data-testid="sidebar-context-menu-${item}"]`).click();
    };

    await menu(beta, "stop");
    await page
      .locator('[data-testid="stop-session-dialog"]')
      .getByRole("button", { name: /^Stop$/ })
      .click();
    await menu(gamma, "start");
    await expect.poll(() => lifecycle, { timeout: 10_000 }).toEqual(["stop sess-b", "start sess-c"]);

    const dialog = await openDeleteDialogFromRow(page, gamma);
    await expect(affected(dialog).list).not.toContainText("sess-a");
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);

    await confirmDelete(await openDeleteDialogFromRow(page, gamma));
    await expect.poll(() => [...handle.trashedIds].sort(), { timeout: 10_000 }).toEqual(["sess-c", "sess-d"]);
  });

  test("permanently deleting one slice keeps the worktree another slice still uses (#4084)", async ({ page }) => {
    const handle = await install(
      page,
      workspace(
        { id: "sess-a", groupPath: "alpha", trashed: false, deleteToTrash: false, cleanableWorktree: true },
        { id: "sess-b", groupPath: "beta", trashed: false },
      ),
    );
    await page.goto("/");

    const dialog = await openDeleteDialogFromRow(page, sessionRows(page).filter({ hasText: "sess-a" }));
    await expect(dialog.locator('[data-testid="delete-session-shared-worktree"]')).toContainText(
      'Worktree and branch are kept: "sess-b" still uses it.',
    );
    await expect(dialog.locator('[data-testid="delete-session-checkbox-worktree"]')).toHaveCount(0);
    await confirmDelete(dialog);
    await expect
      .poll(() => handle.deleteBodies, { timeout: 10_000 })
      .toEqual([expect.objectContaining({ session_ids: ["sess-a"], delete_worktree: false, delete_branch: false })]);
  });

  test("Restore from Trash restores every session of a split workspace (#2533)", async ({ page }) => {
    const handle = await install(
      page,
      workspace(
        { id: "sess-a", groupPath: "alpha", trashed: true },
        { id: "sess-b", groupPath: "beta", trashed: true },
      ),
    );
    await page.goto("/");
    await openTrash(page, 1);

    await trashRows(page).first().locator('[data-testid="sidebar-trash-restore"]').click();
    await expect.poll(() => [...handle.restoredIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
  });

  test("permanent Delete purges every session of a trashed workspace (#2530)", async ({ page }) => {
    const handle = await install(
      page,
      workspace(
        { id: "sess-a", groupPath: "alpha", trashed: true },
        { id: "sess-b", groupPath: "beta", trashed: true },
      ),
    );
    await page.goto("/");
    await openTrash(page);

    const dialog = await openPurgeDialog(page, trashRows(page).first());
    await expect(dialog.locator("#delete-session-dialog-title")).toContainText("Delete Workspace");
    await expect(dialog).toContainText("Permanently delete this workspace?");
    await expect(affected(dialog).count).toContainText("all 2 sessions");
    await expect(affected(dialog).list).toContainText("sess-a");
    await expect(affected(dialog).list).toContainText("sess-b");
    await confirmDelete(dialog);

    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);
    // One request carries the whole workspace; ordering is covered by the Rust order_workspace_deletion tests.
    expect([...(handle.deleteBodies.at(-1)?.session_ids ?? [])].sort()).toEqual(["sess-a", "sess-b"]);
  });
});

// #3167: Empty Trash confirms with the count and purges each trashed workspace
// with one atomic DELETE.
test.describe("Empty Trash", () => {
  async function confirmEmptyTrash(page: Page) {
    await page.locator('[data-testid="sidebar-trash-empty"]').click();
    const dialog = page.locator('[data-testid="empty-trash-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    return dialog;
  }

  test("purges every trashed workspace after confirm (#3167)", async ({ page }) => {
    const handle = await installTrashMocks(page, [
      {
        id: "sess-a",
        branch: "feat/a",
        mainRepoPath: "/tmp/sess-a",
        trashed: true,
        cleanableWorktree: true,
        sandboxed: true,
      },
      { id: "sess-b", branch: "feat/b", mainRepoPath: "/tmp/sess-b", trashed: true },
    ]);
    await page.goto("/");
    await openTrash(page, 2);

    const dialog = await confirmEmptyTrash(page);
    await expect(dialog).toContainText("Permanently delete 2 trashed sessions? This cannot be undone.");
    await dialog.locator('[data-testid="empty-trash-confirm"]').click();

    await expect.poll(() => [...handle.deletedIds].sort(), { timeout: 10_000 }).toEqual(["sess-a", "sess-b"]);

    // Forced like the TUI, with each workspace's cleanup flags.
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
    await expect(trashToggle(page)).toHaveCount(0, { timeout: 10_000 });
  });

  test("a partial failure keeps the failed workspace in Trash and toasts one summary (#3167)", async ({ page }) => {
    const handle = await installTrashMocks(
      page,
      [
        { id: "sess-a", branch: "feat/a", mainRepoPath: "/tmp/sess-a", trashed: true },
        { id: "sess-b", branch: "feat/b", mainRepoPath: "/tmp/sess-b", trashed: true },
      ],
      { failDeleteIds: ["sess-b"] },
    );
    await page.goto("/");
    await openTrash(page, 2);

    const dialog = await confirmEmptyTrash(page);
    await dialog.locator('[data-testid="empty-trash-confirm"]').click();

    // Every workspace is still attempted; one summary toast reports the failure.
    await expect(page.getByRole("alert")).toContainText("Some trashed sessions could not be deleted", {
      timeout: 10_000,
    });
    await expect.poll(() => [...handle.deletedIds], { timeout: 10_000 }).toEqual(["sess-a"]);
    expect(handle.deleteBodies.length).toBe(2);
    await expect(trashToggle(page)).toHaveCount(1, { timeout: 10_000 });
  });
});

// Deleting the session you are viewing: the row disappears and the route falls
// back to the dashboard. The dialog's checkbox-to-DELETE-body mapping is
// covered by the DeleteSessionDialog vitest.
test.describe("Delete active session", () => {
  test("deleting the active session removes the row and falls back to /", async ({ page }) => {
    const handle = await installTrashMocks(page, [
      {
        id: "sess-active",
        title: "story-delete-active",
        projectPath: "/tmp/story",
        groupPath: "/tmp",
        trashed: false,
        deleteToTrash: false,
      },
    ]);
    await page.goto("/session/sess-active");
    await expect(page).toHaveURL(/\/session\/sess-active/);

    const row = sessionRows(page).filter({ hasText: "story-delete-active" }).first();
    await confirmDelete(await openDeleteDialogFromRow(page, row));

    await expect.poll(() => handle.deleteBodies.length, { timeout: 10_000 }).toBe(1);
    await expect(page).not.toHaveURL(/\/session\/sess-active/, { timeout: 10_000 });
    await expect(row).toHaveCount(0, { timeout: 10_000 });
  });
});
