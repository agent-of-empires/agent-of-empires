// Sidebar triage against a real server (#1581): pin, archive, snooze, and bulk archive.

import { spawnSync } from "node:child_process";
import type { Page } from "@playwright/test";
import { test, expect, type ServeHandle } from "../helpers/liveTest";
import { listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";

async function openWithSession(page: Page, spawnServe: () => Promise<ServeHandle>, title: string) {
  const serve = await spawnServe();
  const [session] = await listSessions(serve.baseUrl);
  await page.goto(`${serve.baseUrl}/`);
  const row = page.locator("[data-testid='sidebar-session-row']");
  await expect(row).toContainText(title, { timeout: 10_000 });
  return { serve, sessionId: session!.id, row };
}

const menuItem = (page: Page, name: string) => page.locator(`[data-testid='sidebar-context-menu-${name}']`);
const field = (serve: ServeHandle, key: string) => async () => (await listSessions(serve.baseUrl))[0]?.[key] ?? null;

/** Click a menu item and return the PATCH body sent to `/api/sessions/:id/<endpoint>`. */
async function patchVia(page: Page, sessionId: string, endpoint: string, click: () => Promise<void>) {
  const patch = page.waitForResponse(
    (res) => res.url().endsWith(`/api/sessions/${sessionId}/${endpoint}`) && res.request().method() === "PATCH",
  );
  await click();
  const res = await patch;
  expect(res.ok()).toBe(true);
  return res.request().postDataJSON();
}

test("pin → unpin round-trip lands on the server and survives reload", async ({ page, spawnServe }) => {
  const title = "pin-target";
  const { serve, sessionId, row } = await openWithSession(
    page,
    () => spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) }),
    title,
  );
  await row.click({ button: "right" });
  expect(await patchVia(page, sessionId, "pin", () => menuItem(page, "pin").click())).toEqual({ pinned: true });
  await expect(row.locator("[aria-label='Pinned']")).toBeVisible({ timeout: 5_000 });

  // Archiving or snoozing a pinned session clears the pin server-side, so the menu offers both.
  await row.click({ button: "right" });
  await expect(menuItem(page, "pin")).toContainText("Unpin");
  await expect(menuItem(page, "archive")).toHaveCount(1);
  await expect(menuItem(page, "snooze")).toHaveCount(1);
  await expect.poll(field(serve, "pinned_at"), { timeout: 5_000 }).toBeTruthy();

  await page.reload();
  await expect(row.locator("[aria-label='Pinned']")).toBeVisible({ timeout: 10_000 });
  await row.click({ button: "right" });
  expect(await patchVia(page, sessionId, "pin", () => menuItem(page, "pin").click())).toEqual({ pinned: false });
  await expect.poll(field(serve, "pinned_at"), { timeout: 5_000 }).toBeNull();
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

  await row.click({ button: "right" });
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
  await sunkRow.click({ button: "right" });
  await expect(menuItem(page, "archive")).toContainText("Unarchive");
  await expect(menuItem(page, "pin")).toHaveCount(0);
  await expect(menuItem(page, "snooze")).toHaveCount(0);

  await page.reload();
  await expect.poll(field(serve, "archived_at"), { timeout: 5_000 }).toBeTruthy();
  await expect(sunkSection).toBeVisible({ timeout: 10_000 });
  // The expanded state may be restored from localStorage, so read it rather than toggling blindly.
  if ((await toggle.getAttribute("aria-expanded")) !== "true") await toggle.click();
  await expect(sunkRow).toContainText(title, { timeout: 10_000 });
  await sunkRow.click({ button: "right" });
  expect(await patchVia(page, sessionId, "archive", () => menuItem(page, "archive").click())).toEqual({
    archived: false,
    kill_pane: true,
  });
  await expect.poll(field(serve, "archived_at"), { timeout: 5_000 }).toBeNull();
  await expect(page.locator("[data-testid='sidebar-group-header']")).toHaveCount(1, { timeout: 5_000 });
});

test("snooze preset list + 1h pick + unsnooze round-trip", async ({ page, spawnServe }) => {
  const title = "snooze-target";
  const { serve, sessionId, row } = await openWithSession(
    page,
    () => spawnServe({ seedFn: seedSessionViaAoeAdd({ title }) }),
    title,
  );
  await row.click({ button: "right" });
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
  await snoozedRow.click({ button: "right" });
  await expect(menuItem(page, "unsnooze")).toBeVisible();
  await expect(menuItem(page, "pin")).toHaveCount(0);
  await expect(menuItem(page, "archive")).toHaveCount(0);
  await page.mouse.click(5, 5);

  await snoozedRow.click({ button: "right" });
  expect(await patchVia(page, sessionId, "snooze", () => menuItem(page, "unsnooze").click())).toEqual({
    minutes: null,
  });
  await expect.poll(field(serve, "snoozed_until"), { timeout: 5_000 }).toBeNull();
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

  await rows.nth(0).click({ button: "right" });
  await expect(page.locator("[data-testid='sidebar-context-menu']")).toContainText("2 selected");
  await menuItem(page, "bulk-archive").click();
  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).filter((s) => s.archived_at).length, { timeout: 10_000 })
    .toBe(2);
  await expect(selected).toHaveCount(0, { timeout: 5_000 });
  await expect(page.locator("[data-testid='sidebar-sunk-section']")).toBeVisible({ timeout: 5_000 });
});
