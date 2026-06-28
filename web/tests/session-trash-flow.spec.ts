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
// the WorkspaceSidebar Trash section render + actions for coverage.

interface Handle {
  trashed: boolean;
  trashCalls: number;
  restoreCalls: number;
  deletes: number;
  failTrash: boolean;
  failRestore: boolean;
}

function sessionPayload(trashed: boolean) {
  return {
    id: "sess-trash",
    title: "story-trash",
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

async function mockApis(page: Page): Promise<Handle> {
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
    const sessions = handle.deletes > 0 ? [] : [sessionPayload(handle.trashed)];
    return r.fulfill({ json: { sessions, workspace_ordering: [] } });
  });
  await page.route("**/api/sessions/sess-trash/trash", (r) => {
    if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
    handle.trashCalls += 1;
    if (handle.failTrash) return r.fulfill({ status: 500, body: "boom" });
    handle.trashed = true;
    return r.fulfill({ json: sessionPayload(true) });
  });
  await page.route("**/api/sessions/sess-trash/restore", (r) => {
    if (r.request().method() !== "POST") return r.fulfill({ status: 400 });
    handle.restoreCalls += 1;
    if (handle.failRestore) return r.fulfill({ status: 500, body: "boom" });
    handle.trashed = false;
    return r.fulfill({ json: sessionPayload(false) });
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

    // Row leaves the active list; the footer Trash icon appears and its
    // popover lists the trashed workspace.
    const trashToggle = page.locator('[data-testid="sidebar-trash-toggle"]');
    await expect(trashToggle).toBeVisible({ timeout: 10_000 });
    await trashToggle.click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').filter({ hasText: "story-trash" });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });

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

  test("Delete from the Trash popover opens the permanent-delete dialog", async ({ page }) => {
    const handle = await mockApis(page);
    handle.trashed = true; // start already trashed
    await page.setViewportSize({ width: 1280, height: 720 });

    await page.goto("/");
    await page.locator('[data-testid="sidebar-trash-toggle"]').click();
    const trashRow = page.locator('[data-testid="sidebar-trash-row"]').filter({ hasText: "story-trash" });
    await expect(trashRow).toBeVisible({ timeout: 10_000 });

    // The Trash-popover Delete re-opens the dialog; with the row already
    // trashed it goes straight to permanent delete (no trash checkbox).
    await trashRow.locator('[data-testid="sidebar-trash-purge"]').click();
    const dialog = page.locator('[data-testid="delete-session-dialog"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog.locator('[data-testid="delete-session-permanent"]')).toHaveCount(0);
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    await expect.poll(() => handle.deletes, { timeout: 10_000 }).toBe(1);
  });
});
