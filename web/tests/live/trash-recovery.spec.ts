import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, bootDashboard } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import type { SpawnOptions } from "../helpers/aoeServe";

function pendingTrash(title: string, unverified: boolean, options: { fresh?: boolean } = {}) {
  const fixture = { id: "", profile: "", appDir: "", sentinel: "" };
  const seedFn: NonNullable<SpawnOptions["seedFn"]> = (seed) => {
    seedSessionViaAoeAdd({ title })(seed);
    fixture.appDir = appDirFor(seed.home, seed.xdg, resolveAoeBinary());
    const checkout = join(seed.home, "retained-checkout");
    mkdirSync(checkout);
    fixture.sentinel = join(checkout, "kept.txt");
    writeFileSync(fixture.sentinel, "retained files");
    const profiles = join(fixture.appDir, "profiles");
    for (const profile of readdirSync(profiles)) {
      const path = join(profiles, profile, "sessions.json");
      if (!existsSync(path)) continue;
      const decoded: unknown = JSON.parse(readFileSync(path, "utf8"));
      if (!Array.isArray(decoded)) throw new Error("Expected a native session registry array");
      const rows: unknown[] = decoded;
      const row = rows.find(
        (candidate): candidate is Record<string, unknown> =>
          candidate !== null && typeof candidate === "object" && "title" in candidate && candidate.title === title,
      );
      if (!row) continue;
      if (
        typeof row.id !== "string" ||
        typeof row.lifecycle_generation !== "number" ||
        !Number.isSafeInteger(row.lifecycle_generation)
      )
        throw new Error("Invalid native fixture identity");
      const generation = row.lifecycle_generation + 1;
      const at = new Date(Date.now() - (options.fresh ? 0 : 3_600_000)).toISOString();
      Object.assign(row, {
        project_path: checkout,
        status: "stopped",
        trashed_at: at,
        trash_lifecycle_id: randomUUID(),
        lifecycle_generation: generation,
        lifecycle_reservation: { op: "trash", generation, at },
        terminal_scopes: [],
        pre_trash_project_path: unverified ? checkout : null,
        worktree_info: unverified
          ? {
              branch: "unverified",
              main_repo_path: join(seed.home, "project"),
              managed_by_aoe: true,
              created_at: at,
            }
          : null,
      });
      writeFileSync(path, JSON.stringify(rows));
      fixture.id = row.id;
      fixture.profile = profile;
      break;
    }
    if (!fixture.id) throw new Error("The isolated pending-Trash fixture was not registered");
  };
  return { fixture, seedFn };
}

async function retryThroughApi(baseUrl: string, fixture: { id: string; profile: string }, generation: number) {
  const source = await fetch(`${baseUrl}/api/sessions`);
  const epoch = source.headers.get("aoe-runtime-epoch");
  if (!epoch) throw new Error("The native runtime epoch is missing");
  return fetch(`${baseUrl}/api/sessions/${fixture.id}/trash`, {
    method: "POST",
    headers: { "content-type": "application/json", "aoe-runtime-epoch": epoch },
    body: JSON.stringify({ kill_pane: true, expected_profile: fixture.profile, expected_generation: generation }),
  });
}

for (const unverified of [false, true]) {
  test(`Trash retry ${unverified ? "retains unverified files and remains recoverable" : "completes a recoverable transition"}`, async ({
    page,
    spawnServe,
  }) => {
    const { fixture, seedFn } = pendingTrash("RetryWeb", unverified);
    const serve = await spawnServe({ seedFn });
    const before = (await listSessions(serve.baseUrl)).find((row) => row.id === fixture.id)!;
    await bootDashboard(page, serve);
    await page.getByTestId("sidebar-trash-toggle").click();
    const row = page.getByTestId("sidebar-trash-row");
    await expect(row.getByTestId("sidebar-trash-retry")).toBeVisible();
    await expect(row.getByTestId("sidebar-trash-purge")).toHaveCount(0);
    const response = page.waitForResponse(
      (result) =>
        new URL(result.url()).pathname === `/api/sessions/${fixture.id}/trash` && result.request().method() === "POST",
    );
    await row.getByTestId("sidebar-trash-retry").click();
    expect((await response).status()).toBe(200);
    await expect
      .poll(async () => {
        const current = (await listSessions(serve.baseUrl)).find((candidate) => candidate.id === fixture.id);
        return current?.lifecycle_generation;
      })
      .toBe(Number(before.lifecycle_generation) + 1);
    if (unverified) {
      await expect(row.getByTestId("sidebar-trash-retry")).toBeEnabled();
      await expect(row.getByTestId("sidebar-trash-purge")).toHaveCount(0);
      await page.evaluate(() => {
        const state = Object.assign(window, { retryPosts: 0 });
        const original = window.fetch;
        window.fetch = (...args) => {
          const url = args[0] instanceof Request ? args[0].url : String(args[0]);
          if (args[1]?.method === "POST" && new URL(url, location.href).pathname.endsWith("/trash"))
            state.retryPosts += 1;
          return original(...args);
        };
      });
      const retry = row.getByTestId("sidebar-trash-retry");
      const originalButton = (await retry.elementHandle())!;
      const box = (await retry.boundingBox())!;
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
      const concurrent = await retryThroughApi(serve.baseUrl, fixture, Number(before.lifecycle_generation) + 1);
      expect(concurrent.status).toBe(200);
      await originalButton.waitForElementState("hidden");
      await page.mouse.up();
      expect(await page.evaluate(() => ("retryPosts" in window ? window.retryPosts : undefined))).toBe(0);
      const second = page.waitForResponse(
        (result) =>
          new URL(result.url()).pathname === `/api/sessions/${fixture.id}/trash` &&
          result.request().method() === "POST",
      );
      await retry.click();
      expect((await second).status()).toBe(200);
      await expect
        .poll(
          async () =>
            (await listSessions(serve.baseUrl)).find((candidate) => candidate.id === fixture.id)?.lifecycle_generation,
        )
        .toBe(Number(before.lifecycle_generation) + 3);
      await expect(retry).toBeEnabled();
      const retained = (await listSessions(serve.baseUrl)).find((candidate) => candidate.id === fixture.id)!;
      expect(retained.lifecycle_reservation).toMatchObject({
        op: "trash",
        generation: Number(before.lifecycle_generation) + 3,
      });
      const source = await fetch(`${serve.baseUrl}/api/sessions`);
      const epoch = source.headers.get("aoe-runtime-epoch");
      if (!epoch) throw new Error("The native runtime epoch is missing");
      const purge = await fetch(`${serve.baseUrl}/api/sessions/${fixture.id}`, {
        method: "DELETE",
        headers: { "content-type": "application/json", "aoe-runtime-epoch": epoch },
        body: JSON.stringify({}),
      });
      expect(purge.status).toBe(409);
      const afterPurge = (await listSessions(serve.baseUrl)).find((candidate) => candidate.id === fixture.id)!;
      expect(afterPurge.lifecycle_generation).toBe(retained.lifecycle_generation);
      expect(afterPurge.lifecycle_reservation).toEqual(retained.lifecycle_reservation);
    } else {
      await expect(row.getByTestId("sidebar-trash-retry")).toHaveCount(0);
      await expect(row.getByTestId("sidebar-trash-restore")).toBeVisible();
    }
    expect(readFileSync(fixture.sentinel, "utf8")).toBe("retained files");
  });
}

test("Fresh Trash owner refuses Retry without changing custody", async ({ page, spawnServe }) => {
  const { fixture, seedFn } = pendingTrash("FreshOwner", true, { fresh: true });
  const serve = await spawnServe({ seedFn });
  const before = (await listSessions(serve.baseUrl)).find((row) => row.id === fixture.id)!;
  const journalPath = join(fixture.appDir, "pending-purge-owners.json");
  const journalBefore = readFileSync(journalPath);
  await bootDashboard(page, serve);
  await page.getByTestId("sidebar-trash-toggle").click();
  const response = page.waitForResponse(
    (result) =>
      new URL(result.url()).pathname === `/api/sessions/${fixture.id}/trash` && result.request().method() === "POST",
  );
  await page.getByTestId("sidebar-trash-retry").click();
  const refused = await response;
  expect(refused.status()).toBe(409);
  expect(refused.headers()["aoe-error-code"]).toBe("lifecycle_locked");
  const after = (await listSessions(serve.baseUrl)).find((row) => row.id === fixture.id)!;
  expect(after.lifecycle_generation).toBe(before.lifecycle_generation);
  expect(after.lifecycle_reservation).toEqual(before.lifecycle_reservation);
  expect(after.trash_lifecycle_id).toBe(before.trash_lifecycle_id);
  expect(after.trashed_at).toBe(before.trashed_at);
  expect(readFileSync(journalPath)).toEqual(journalBefore);
  expect(readFileSync(fixture.sentinel, "utf8")).toBe("retained files");
});
