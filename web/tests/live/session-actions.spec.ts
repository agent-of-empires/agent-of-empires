// Sidebar session context menu against a real server: rename, group, delete, trash, fork.

import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { startAcpSession } from "../helpers/acp";

/** Seed one session, open the dashboard, and wait for its row. */
async function openWithSession(
  page: Page,
  spawnServe: (opts?: { seedFn?: ReturnType<typeof seedSessionViaAoeAdd> }) => Promise<ServeHandle>,
  title: string,
) {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) });
  const [session] = await listSessions(serve.baseUrl);
  await page.goto(`${serve.baseUrl}/`);
  const row = page.locator("[data-testid='sidebar-session-row']");
  // Cold parallel servers can paint the first row slowly.
  await expect(row).toContainText(title, { timeout: 10_000 });
  return { serve, sessionId: session!.id, row };
}

/** Count fetches with `method` issued by the page from now on. */
async function countFetches(page: Page, method: string) {
  await page.evaluate((m) => {
    const w = window as unknown as { __mutationCalls: number };
    const original = window.fetch;
    w.__mutationCalls = 0;
    window.fetch = (...args) => {
      if (args[1]?.method === m) w.__mutationCalls += 1;
      return original(...args);
    };
  }, method);
  return () => page.evaluate(() => (window as unknown as { __mutationCalls: number }).__mutationCalls);
}

const menuItem = (page: Page, name: string) => page.locator(`[data-testid='sidebar-context-menu-${name}']`);
const firstSession = async (serve: ServeHandle) => (await listSessions(serve.baseUrl))[0];

test.describe("rename (#1220)", () => {
  // #2624: real Claude titles with shell metacharacters used to break.
  for (const updated of ["rename-target", "Goal: I've fixed the parser, right?"]) {
    test(`Enter commits "${updated}" through PATCH /api/sessions/:id`, async ({ page, spawnServe }) => {
      const { serve, sessionId, row } = await openWithSession(page, spawnServe, "rename-source");
      await row.click({ button: "right" });
      const patch = page.waitForResponse(
        (res) => res.url().endsWith(`/api/sessions/${sessionId}`) && res.request().method() === "PATCH",
      );
      await menuItem(page, "rename").click();
      const input = page.locator("[data-testid='sidebar-rename-input']");
      await expect(input).toBeVisible();
      await input.fill(updated);
      await input.press("Enter");

      const patchRes = await patch;
      expect(patchRes.ok(), `rename should succeed, got ${patchRes.status()}`).toBe(true);
      expect(patchRes.request().postDataJSON()).toEqual({ title: updated });
      await expect(page.getByText(updated)).toBeVisible({ timeout: 5_000 });
      await expect.poll(async () => (await firstSession(serve))?.title, { timeout: 5_000 }).toBe(updated);
    });
  }

  for (const c of [
    { name: "Escape cancels mid-edit, no PATCH fires", value: "should-not-stick", key: "Escape" },
    { name: "blank title is rejected by commitRename without firing PATCH", value: "   ", key: "Enter" },
  ]) {
    test(c.name, async ({ page, spawnServe }) => {
      const { row } = await openWithSession(page, spawnServe, "rename-kept");
      const patches = await countFetches(page, "PATCH");
      await row.click({ button: "right" });
      await menuItem(page, "rename").click();
      const input = page.locator("[data-testid='sidebar-rename-input']");
      await input.fill(c.value);
      await input.press(c.key);
      await expect(input).toBeHidden();
      await expect(row).toContainText("rename-kept");
      expect(await patches()).toBe(0);
    });
  }
});

test.describe("group edit (#1726)", () => {
  async function saveGroup(page: Page, sessionId: string, value: string, prefilled = "") {
    const patch = page.waitForResponse(
      (res) => res.url().endsWith(`/api/sessions/${sessionId}/group`) && res.request().method() === "PATCH",
    );
    await menuItem(page, "edit-group").click();
    const modal = page.locator("[data-testid='session-group-modal']");
    await expect(modal).toBeVisible();
    const input = modal.locator("[data-testid='session-group-modal-input']");
    await expect(input).toHaveValue(prefilled);
    await input.fill(value);
    await modal.locator("[data-testid='session-group-modal-save']").click();
    const patchRes = await patch;
    expect(patchRes.ok(), `group save should succeed, got ${patchRes.status()}`).toBe(true);
    expect(patchRes.request().postDataJSON()).toEqual({ group: value });
    await expect(modal).toBeHidden();
  }

  // #2624: apostrophes in group paths are accepted.
  for (const group of ["team/alpha", "Sam's Team/imports"]) {
    test(`Save commits new group "${group}", creating the group`, async ({ page, spawnServe }) => {
      const { serve, sessionId, row } = await openWithSession(page, spawnServe, "group-edit-new");
      expect((await firstSession(serve))?.group_path).toBe("");
      await row.click({ button: "right" });
      await saveGroup(page, sessionId, group);
      await expect.poll(async () => (await firstSession(serve))?.group_path, { timeout: 5_000 }).toBe(group);
      const groups = (await fetch(`${serve.baseUrl}/api/groups`).then((r) => r.json())) as Array<{ path: string }>;
      expect(groups.map((g) => g.path)).toContain(group);
    });
  }

  test("clearing the field commits an empty group, ungrouping the session", async ({ page, spawnServe }) => {
    const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "group-edit-clear" }) });
    const sessionId = (await firstSession(serve))!.id;
    const setRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/group`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ group: "team/beta" }),
    });
    expect(setRes.ok).toBe(true);

    await page.goto(`${serve.baseUrl}/`);
    const row = page.locator("[data-testid='sidebar-session-row']");
    await expect(row).toContainText("group-edit-clear", { timeout: 10_000 });
    await row.click({ button: "right" });
    await saveGroup(page, sessionId, "", "team/beta");
    await expect.poll(async () => (await firstSession(serve))?.group_path, { timeout: 5_000 }).toBe("");
  });
});

test.describe("delete and trash (#1220, #2489)", () => {
  async function openDeleteDialog(page: Page, row: import("@playwright/test").Locator) {
    await row.click({ button: "right" });
    await menuItem(page, "delete").click();
    const dialog = page.locator("[data-testid='delete-session-dialog']");
    await expect(dialog).toBeVisible();
    return dialog;
  }

  test("Delete permanently fires DELETE /api/workspaces and removes the row", async ({ page, spawnServe }) => {
    const { serve, sessionId, row } = await openWithSession(page, spawnServe, "delete-me");
    const dialog = await openDeleteDialog(page, row);
    // Trash is the default, so opt into the purge path.
    await dialog.locator("[data-testid='delete-session-permanent']").click();
    const deletePromise = page.waitForResponse(
      (res) => res.url().endsWith(`/api/workspaces`) && res.request().method() === "DELETE",
    );
    await dialog.getByRole("button", { name: /^Delete$/ }).click();

    const deleteRes = await deletePromise;
    expect(deleteRes.ok()).toBe(true);
    // `aoe add` makes an attached session, so every cleanup flag is false.
    expect(deleteRes.request().postDataJSON()).toEqual({
      session_ids: [sessionId],
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
      force_delete: false,
    });
    await expect.poll(async () => (await listSessions(serve.baseUrl)).length, { timeout: 10_000 }).toBe(0);
    await expect(row).toHaveCount(0, { timeout: 10_000 });
  });

  for (const dismiss of ["Cancel", "Escape"]) {
    test(`${dismiss} closes the dialog without firing DELETE`, async ({ page, spawnServe }) => {
      const { serve, row } = await openWithSession(page, spawnServe, "keeps-me");
      const deletes = await countFetches(page, "DELETE");
      const dialog = await openDeleteDialog(page, row);
      if (dismiss === "Cancel") await dialog.getByRole("button", { name: "Cancel" }).click();
      else await page.keyboard.press("Escape");
      await expect(dialog).toBeHidden();
      expect(await deletes()).toBe(0);
      const sessions = await listSessions(serve.baseUrl);
      expect(sessions).toHaveLength(1);
      expect(sessions[0]!.title).toBe("keeps-me");
    });
  }

  test("Move to Trash hides the row, Restore brings it back", async ({ page, spawnServe }) => {
    const { serve, sessionId, row } = await openWithSession(page, spawnServe, "trash-me");
    const dialog = await openDeleteDialog(page, row);
    await expect(dialog.locator("[data-testid='delete-session-permanent']")).toBeVisible();
    const trashed = async () => {
      const ss = await listSessions(serve.baseUrl);
      return ss.length === 1 && ss[0]!.trashed_at != null;
    };

    const trashPromise = page.waitForResponse(
      (res) => res.url().endsWith(`/api/sessions/${sessionId}/trash`) && res.request().method() === "POST",
    );
    await dialog.getByRole("button", { name: /^Delete$/ }).click();
    expect((await trashPromise).ok()).toBe(true);
    await expect.poll(trashed).toBe(true);
    await expect(row).toHaveCount(0, { timeout: 10_000 });

    const trashToggle = page.locator("[data-testid='sidebar-trash-toggle']");
    await expect(trashToggle).toHaveAttribute("aria-label", "Trash (1)");
    await trashToggle.click();
    const restorePromise = page.waitForResponse(
      (res) => res.url().endsWith(`/api/sessions/${sessionId}/restore`) && res.request().method() === "POST",
    );
    await page.locator("[data-testid='sidebar-trash-restore']").click();
    expect((await restorePromise).ok()).toBe(true);
    await expect.poll(async () => (await listSessions(serve.baseUrl)).length === 1 && !(await trashed())).toBe(true);
    await expect(row).toContainText("trash-me", { timeout: 10_000 });
  });
});

// Fork is gated on the server's `acp_can_fork`; the session/fork handshake is covered by tests/e2e.
test.describe("fork", () => {
  test("Fork action is hidden until the session has a captured ACP session id", async ({ page, spawnServe }) => {
    const { row } = await openWithSession(page, (opts) => spawnServe({ acp: true, ...opts }), "fork-hidden-source");
    await row.click({ button: "right" });
    const menu = page.locator("[data-testid='sidebar-context-menu']");
    await expect(menu).toBeVisible();
    await expect(menu.locator("[data-testid='sidebar-context-menu-fork']")).toHaveCount(0);
  });

  test("forks a structured session into a distinct child, parent untouched", async ({ page, spawnServe }) => {
    const title = "fork-source";
    const { serve, sessionId: parentId } = await startAcpSession(spawnServe, { title, tool: "claude" });
    await expect
      .poll(
        async () => {
          const id = (await listSessions(serve.baseUrl)).find((s) => s.id === parentId)?.acp_session_id;
          return typeof id === "string" && id.length > 0;
        },
        {
          timeout: 10_000,
          intervals: [100, 200, 500, 1000],
          message: "parent session should expose acp_session_id before sidebar fork",
        },
      )
      .toBe(true);

    await page.goto(`${serve.baseUrl}/`);
    const row = page.locator("[data-testid='sidebar-session-row']");
    await expect(row).toContainText(title, { timeout: 10_000 });
    await row.click({ button: "right" });
    const menu = page.locator("[data-testid='sidebar-context-menu']");
    await expect(menu).toBeVisible();
    const forkButton = menu.locator("[data-testid='sidebar-context-menu-fork']");
    await expect(forkButton).toBeVisible({ timeout: 10_000 });

    const createPromise = page.waitForResponse(
      (res) => res.url().endsWith("/api/sessions") && res.request().method() === "POST",
    );
    await forkButton.click();
    const createRes = await createPromise;
    expect(createRes.ok(), `fork create failed: ${createRes.status()}`).toBe(true);
    expect(createRes.request().postDataJSON()).toMatchObject({ view: "structured", tool: "claude" });
    const childId: string = (await createRes.json()).id;
    expect(childId).toBeTruthy();
    expect(childId).not.toBe(parentId);

    await expect
      .poll(async () => (await listSessions(serve.baseUrl)).map((s) => s.id), { timeout: 10_000 })
      .toEqual(expect.arrayContaining([parentId, childId]));
    expect((await listSessions(serve.baseUrl)).find((s) => s.id === parentId)?.title).toBe(title);
  });
});
