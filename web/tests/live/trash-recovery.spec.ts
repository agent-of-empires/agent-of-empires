import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { test, expect, bootDashboard } from "../helpers/liveTest";
import { appDirFor, listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import type { SpawnOptions } from "../helpers/aoeServe";

function pendingTrash(title: string, unverified: boolean) {
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
      const at = new Date(Date.now() - 3_600_000).toISOString();
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
      await expect(row.getByTestId("sidebar-trash-forget")).toBeVisible();
      await expect(row.getByTestId("sidebar-trash-purge")).toHaveCount(0);
    } else {
      await expect(row.getByTestId("sidebar-trash-retry")).toHaveCount(0);
      await expect(row.getByTestId("sidebar-trash-restore")).toBeVisible();
    }
    expect(readFileSync(fixture.sentinel, "utf8")).toBe("retained files");
  });
}

for (const mouse of [false, true]) {
  test(`Forget Trash preserves retained files with ${mouse ? "mouse" : "keyboard"} confirmation`, async ({
    page,
    spawnServe,
  }, testInfo) => {
    const { fixture, seedFn } = pendingTrash("ForgetWeb", true);
    const serve = await spawnServe({ seedFn });
    await bootDashboard(page, serve);
    await page.getByTestId("sidebar-trash-toggle").click();
    await page.getByTestId("sidebar-trash-forget").click();
    const dialog = page.getByTestId("forget-trash-dialog");
    await expect(dialog).toBeVisible();
    const screenshot = testInfo.outputPath("trash-forget-confirmation.png");
    await page.screenshot({ path: screenshot });
    await testInfo.attach("Forget Trash confirmation", { path: screenshot, contentType: "image/png" });
    const response = page.waitForResponse(
      (result) =>
        new URL(result.url()).pathname === `/api/sessions/${fixture.id}/trash/forget` &&
        result.request().method() === "POST",
    );
    if (mouse) await dialog.getByRole("button", { name: "Forget record", exact: true }).click();
    else await page.keyboard.press("Enter");
    expect((await response).status()).toBe(204);
    await expect.poll(async () => (await listSessions(serve.baseUrl)).some((row) => row.id === fixture.id)).toBe(false);
    await expect(page.getByTestId("sidebar-trash-toggle")).toHaveCount(0);
    expect(readFileSync(fixture.sentinel, "utf8")).toBe("retained files");
    const journal = JSON.parse(readFileSync(join(fixture.appDir, "pending-purge-owners.json"), "utf8"));
    expect(journal.owners).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ session_id: fixture.id, purpose: expect.objectContaining({ kind: "retained" }) }),
      ]),
    );
  });

  test(`Concurrent retry invalidates ${mouse ? "mouse" : "keyboard"} Forget consent`, async ({ page, spawnServe }) => {
    const { fixture, seedFn } = pendingTrash("StaleForgetWeb", true);
    const serve = await spawnServe({ seedFn });
    await bootDashboard(page, serve);
    await page.evaluate(() => {
      const state = Object.assign(window, { forgetPosts: 0 });
      const original = window.fetch;
      window.fetch = (...args) => {
        const url = args[0] instanceof Request ? args[0].url : String(args[0]);
        if (args[1]?.method === "POST" && new URL(url, location.href).pathname.endsWith("/trash/forget")) {
          state.forgetPosts += 1;
        }
        return original(...args);
      };
    });
    await page.getByTestId("sidebar-trash-toggle").click();
    await page.getByTestId("sidebar-trash-forget").click();
    const dialog = page.getByTestId("forget-trash-dialog");
    await expect(dialog).toBeVisible();
    if (mouse) {
      const box = (await dialog.getByRole("button", { name: "Forget record", exact: true }).boundingBox())!;
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
    }
    const source = await fetch(`${serve.baseUrl}/api/sessions`);
    const epoch = source.headers.get("aoe-runtime-epoch");
    if (!epoch) throw new Error("The native runtime epoch is missing");
    const current = (await listSessions(serve.baseUrl)).find((row) => row.id === fixture.id)!;
    const retry = await fetch(`${serve.baseUrl}/api/sessions/${fixture.id}/trash`, {
      method: "POST",
      headers: { "content-type": "application/json", "aoe-runtime-epoch": epoch },
      body: JSON.stringify({
        kill_pane: true,
        expected_profile: fixture.profile,
        expected_generation: current.lifecycle_generation,
      }),
    });
    expect(retry.status).toBe(200);
    await expect(dialog).toHaveCount(0);
    if (mouse) await page.mouse.up();
    else await page.keyboard.press("Enter");
    expect(await page.evaluate(() => ("forgetPosts" in window ? window.forgetPosts : undefined))).toBe(0);
    const retained = (await listSessions(serve.baseUrl)).find((row) => row.id === fixture.id)!;
    expect(retained.lifecycle_generation).toBe(Number(current.lifecycle_generation) + 1);
    expect(readFileSync(fixture.sentinel, "utf8")).toBe("retained files");
  });
}
