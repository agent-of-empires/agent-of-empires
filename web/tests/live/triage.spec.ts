// Sidebar triage against a real server (#1581): pin, archive, snooze, bulk archive, and the
// managed-session actions of #5833 (ignore, take ownership).

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import type { Locator, Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";

async function openWithSession(page: Page, spawnServe: () => Promise<ServeHandle>, title: string) {
  const serve = await spawnServe();
  const [session] = await listSessions(serve.baseUrl);
  await page.goto(`${serve.baseUrl}/`);
  const row = page.locator("[data-testid='sidebar-session-row']");
  await expect(row).toContainText(title, { timeout: 10_000 });
  return { serve, sessionId: session!.id, row };
}

const contextMenu = (page: Page) => page.locator("[data-testid='sidebar-context-menu']");
const menuItem = (page: Page, name: string) => page.locator(`[data-testid='sidebar-context-menu-${name}']`);

/** Open the row menu and expand its More disclosure, where the managed-session actions live. */
async function openMoreMenu(page: Page, target: Locator, item: string) {
  await openMenu(page, target);
  await menuItem(page, "more").click();
  await expect(menuItem(page, item)).toBeVisible();
}
const field = (serve: ServeHandle, key: string) => async () => (await listSessions(serve.baseUrl))[0]?.[key] ?? null;

/** Right-click `target` once any previous menu has finished closing, and wait for the new one. */
async function openMenu(page: Page, target: Locator) {
  await expect(contextMenu(page)).toBeHidden();
  await target.click({ button: "right" });
  await expect(contextMenu(page)).toBeVisible();
}

async function closeMenu(page: Page) {
  // The menu handles Escape only while focus is inside it.
  await contextMenu(page).locator("button:visible").first().focus();
  await page.keyboard.press("Escape");
  await expect(contextMenu(page)).toBeHidden();
}

/** Click a menu item and return the PATCH body sent to `/api/sessions/:id/<endpoint>`. */
async function patchVia(page: Page, sessionId: string, endpoint: string, click: () => Promise<void>) {
  return await writeVia(page, sessionId, endpoint, "PATCH", click);
}

/** The same for a POST, used by the managed-session actions. */
async function postVia(page: Page, sessionId: string, endpoint: string, click: () => Promise<void>) {
  return await writeVia(page, sessionId, endpoint, "POST", click);
}

async function writeVia(
  page: Page,
  sessionId: string,
  endpoint: string,
  method: "PATCH" | "POST",
  click: () => Promise<void>,
) {
  const write = page.waitForResponse(
    (res) => res.url().endsWith(`/api/sessions/${sessionId}/${endpoint}`) && res.request().method() === method,
  );
  await click();
  const res = await write;
  expect(res.ok()).toBe(true);
  return res.request().postDataJSON();
}

test("pin → unpin survives reload; snooze presets, a 1h pick, and unsnooze round-trip", async ({
  page,
  spawnServe,
}) => {
  const title = "pin-target";
  const { serve, sessionId, row } = await openWithSession(
    page,
    () => spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) }),
    title,
  );
  await openMenu(page, row);
  expect(await patchVia(page, sessionId, "pin", () => menuItem(page, "pin").click())).toEqual({ pinned: true });
  // Pin toggles in place and leaves the menu open over the row.
  await closeMenu(page);
  await expect(row.locator("[aria-label='Pinned']")).toBeVisible({ timeout: 5_000 });

  // Archiving or snoozing a pinned session clears the pin server-side, so the menu offers both.
  await openMenu(page, row);
  await expect(menuItem(page, "pin")).toHaveAttribute("aria-pressed", "true");
  await expect(menuItem(page, "archive")).toHaveCount(1);
  await expect(menuItem(page, "snooze")).toHaveCount(1);
  await expect.poll(field(serve, "pinned_at"), { timeout: 5_000 }).toBeTruthy();

  await page.reload();
  await expect(row.locator("[aria-label='Pinned']")).toBeVisible({ timeout: 10_000 });
  await openMenu(page, row);
  expect(await patchVia(page, sessionId, "pin", () => menuItem(page, "pin").click())).toEqual({ pinned: false });
  await closeMenu(page);
  await expect.poll(field(serve, "pinned_at"), { timeout: 5_000 }).toBeNull();

  // Snooze: the TUI preset list, a 1h pick, and an unsnooze round trip.
  await openMenu(page, row);
  await menuItem(page, "snooze").click();
  const modal = page.locator("[data-testid='snooze-modal']");
  await expect(modal).toBeVisible();
  // Matches the TUI presets in src/tui/dialogs/snooze_duration.rs.
  for (const m of [60, 120, 180, 240, 300, 360, 1440, 10080]) {
    await expect(modal.locator(`[data-testid='snooze-modal-preset-${m}']`)).toBeVisible();
  }

  const issuedAt = Date.now();
  expect(
    await patchVia(page, sessionId, "snooze", () => modal.locator("[data-testid='snooze-modal-preset-60']").click()),
  ).toEqual({ minutes: 60 });
  await expect
    .poll(
      async () => {
        const ts = (await field(serve, "snoozed_until")()) as string | null;
        return ts ? Date.parse(ts) : null;
      },
      { timeout: 5_000 },
    )
    .toBeGreaterThan(issuedAt + 55 * 60_000);

  const sunkSection = page.locator("[data-testid='sidebar-sunk-section']");
  await expect(sunkSection).toBeVisible({ timeout: 5_000 });
  await sunkSection.locator("[data-testid='sidebar-sunk-toggle']").click();
  const snoozedRow = sunkSection.locator("[data-testid='sidebar-session-row']");
  await expect(snoozedRow).toContainText(title);
  await expect(snoozedRow.locator("[aria-label='Snoozed']")).toBeVisible();
  // A snoozed row offers only Unsnooze.
  await openMenu(page, snoozedRow);
  await expect(menuItem(page, "unsnooze")).toBeVisible();
  await expect(menuItem(page, "pin")).toHaveCount(0);
  await expect(menuItem(page, "archive")).toHaveCount(0);
  await closeMenu(page);

  await openMenu(page, snoozedRow);
  expect(await patchVia(page, sessionId, "snooze", () => menuItem(page, "unsnooze").click())).toEqual({
    minutes: null,
  });
  await expect.poll(field(serve, "snoozed_until"), { timeout: 5_000 }).toBeNull();
});

test("archive sinks the row into the collapsible footer and persists", async ({ page, spawnServe }) => {
  const title = "archive-target";
  const { serve, sessionId, row } = await openWithSession(
    page,
    () => spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) }),
    title,
  );
  const sunkSection = page.locator("[data-testid='sidebar-sunk-section']");
  const sunkRow = sunkSection.locator("[data-testid='sidebar-session-row']");
  const toggle = sunkSection.locator("[data-testid='sidebar-sunk-toggle']");

  await openMenu(page, row);
  expect(await patchVia(page, sessionId, "archive", () => menuItem(page, "archive").click())).toEqual({
    archived: true,
    kill_pane: true,
  });
  await expect.poll(field(serve, "archived_at"), { timeout: 5_000 }).toBeTruthy();

  // #1868: the agent's tmux session is killed. List the pinned socket, or an empty default passes trivially.
  await expect
    .poll(
      () => {
        const result = spawnSync("tmux", ["-S", serve.tmuxSocket, "ls"], { env: serve.env, encoding: "utf8" });
        // Exit status 1 just means no sessions.
        if (result.error) throw result.error;
        return result.stdout ?? "";
      },
      { timeout: 5_000 },
    )
    .not.toContain(sessionId.slice(0, 8));

  await expect(sunkSection).toBeVisible({ timeout: 5_000 });
  // #1600: a repo group whose only workspace sank loses its header.
  await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(0);
  // Collapsed by default.
  await expect(sunkRow).toHaveCount(0);
  await toggle.click();
  await expect(sunkRow).toContainText(title);
  await expect(sunkRow.locator("[aria-label='Archived']")).toBeVisible();
  // An archived row offers only Unarchive.
  await openMenu(page, sunkRow);
  await expect(menuItem(page, "archive")).toContainText("Unarchive");
  await expect(menuItem(page, "pin")).toHaveCount(0);
  await expect(menuItem(page, "snooze")).toHaveCount(0);

  await page.reload();
  await expect.poll(field(serve, "archived_at"), { timeout: 5_000 }).toBeTruthy();
  await expect(sunkSection).toBeVisible({ timeout: 10_000 });
  // The expanded state may be restored from localStorage, so read it rather than toggling blindly.
  if ((await toggle.getAttribute("aria-expanded")) !== "true") await toggle.click();
  await expect(sunkRow).toContainText(title, { timeout: 10_000 });
  await openMenu(page, sunkRow);
  expect(await patchVia(page, sessionId, "archive", () => menuItem(page, "archive").click())).toEqual({
    archived: false,
    kill_pane: true,
  });
  await expect.poll(field(serve, "archived_at"), { timeout: 5_000 }).toBeNull();
  await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(1, { timeout: 5_000 });
});

test("selecting two rows and bulk-archiving persists both", async ({ page, spawnServe }) => {
  // #1724, #2312: bulk actions live in the selected rows' context menu.
  const serve = await spawnServe({
    seedFn: (env) => {
      seedSessionViaAoeAdd({ title: "alpha", subdir: "proj-a" })(env);
      seedSessionViaAoeAdd({ title: "beta", subdir: "proj-b" })(env);
    },
  });
  expect(await listSessions(serve.baseUrl)).toHaveLength(2);
  await page.goto(`${serve.baseUrl}/`);
  const rows = page.locator("[data-testid='sidebar-session-row']");
  await expect(rows).toHaveCount(2, { timeout: 10_000 });

  await rows.nth(0).click({ modifiers: ["ControlOrMeta"] });
  await rows.nth(1).click({ modifiers: ["ControlOrMeta"] });
  expect(page.url()).not.toContain("/session/");
  const selected = page.locator("[data-testid='sidebar-session-row'][data-selected]");
  await expect(selected).toHaveCount(2);

  await openMenu(page, rows.nth(0));
  await expect(contextMenu(page)).toContainText("2 selected");
  await menuItem(page, "bulk-archive").click();
  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).filter((s) => s.archived_at).length, { timeout: 10_000 })
    .toBe(2);
  await expect(selected).toHaveCount(0, { timeout: 5_000 });
  await expect(page.locator("[data-testid='sidebar-sunk-section']")).toBeVisible({ timeout: 5_000 });
});

test("ignore keeps a managed row live, unignore restores it, and take ownership clears the manager link", async ({
  page,
  spawnServe,
}) => {
  // #5833: both actions are menu-only, so the menu, the REST call and the row's own state are
  // what a person actually touches. `aoe add -P` is the documented way to make a managed row.
  const childTitle = "managed-child";
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "manager" }) });
  const [parent] = await listSessions(serve.baseUrl);
  const added = spawnSync(
    resolveAoeBinary(),
    ["add", join(serve.home, "project"), "-t", childTitle, "-c", "claude", "-P", parent!.id],
    { env: serve.env },
  );
  expect(added.status, added.stderr?.toString()).toBe(0);

  const child = () => listSessions(serve.baseUrl).then((rows) => rows.find((s) => s.title === childTitle));
  // The response omits an unset marker, so read it as null rather than undefined.
  const childField = (key: string) => async () => (await child())?.[key] ?? null;
  await expect.poll(childField("parent_session_id"), { timeout: 10_000 }).toBe(parent!.id);
  const childId = (await child())!.id;

  await page.goto(`${serve.baseUrl}/`);
  const row = page.locator("[data-testid='sidebar-session-row']", { hasText: childTitle });
  await expect(row).toBeVisible({ timeout: 10_000 });

  await openMoreMenu(page, row, "ignore");
  await expect(menuItem(page, "ignore")).toContainText("Ignore managed session");
  await expect(menuItem(page, "reclaim-ownership")).toContainText("Take ownership back");
  expect(await patchVia(page, childId, "ignored", () => menuItem(page, "ignore").click())).toEqual({
    ignored: true,
  });
  await expect.poll(childField("ignored_at"), { timeout: 5_000 }).toBeTruthy();

  // An ignored row is still a live session: it keeps its own row and stays out of the footer.
  await expect(row).toBeVisible();
  await expect(page.locator("[data-testid='sidebar-sunk-section']")).toHaveCount(0);
  // The sidebar learns the new marker from its poll, and the menu then offers the inverse.
  await openMoreMenu(page, row, "ignore");
  await expect(menuItem(page, "ignore")).toContainText("Unignore session", { timeout: 10_000 });
  expect(await patchVia(page, childId, "ignored", () => menuItem(page, "ignore").click())).toEqual({
    ignored: false,
  });
  await expect.poll(childField("ignored_at"), { timeout: 5_000 }).toBeNull();

  // Take ownership clears the manager link and the ignore marker in one write, and the row drops
  // both managed actions afterwards.
  await openMoreMenu(page, row, "ignore");
  await expect(menuItem(page, "ignore")).toContainText("Ignore managed session", { timeout: 10_000 });
  expect(await postVia(page, childId, "reclaim-ownership", () => menuItem(page, "reclaim-ownership").click())).toEqual({
    reclaim: true,
  });
  await expect.poll(childField("parent_session_id"), { timeout: 5_000 }).toBeNull();
  await openMenu(page, row);
  await expect(menuItem(page, "ignore")).toHaveCount(0, { timeout: 10_000 });
  await expect(menuItem(page, "reclaim-ownership")).toHaveCount(0);
  await expect(row).toBeVisible();
});
