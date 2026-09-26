// Moving between sessions: sidebar select focus, command palette, terminal chord.

import { spawnSync } from "node:child_process";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { test, expect } from "../../helpers/liveTest";
import { listSessions, resolveAoeBinary, seedSessionViaAoeAdd } from "../../helpers/aoeServe";
import { composer, startAcpSession, waitForStructuredView } from "../../helpers/acp";
import { initWorkingRepo } from "../../helpers/gitFixture";

const MOD = process.platform === "darwin" ? "Meta" : "Control";

const blurActive = (page: Page) => page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());

async function firstSidebarRow(page: Page, baseUrl: string) {
  await page.goto(baseUrl);
  const row = page.locator('[data-testid="sidebar-session-row"]').first();
  await expect(row).toBeVisible({ timeout: 10_000 });
  return row;
}

// #1454: re-selecting the active session neither remounts nor reconnects, so only the select dispatch refocuses.
test("desktop: re-selecting the active structured view session refocuses the composer", async ({
  page,
  spawnServe,
}) => {
  const { serve } = await startAcpSession(spawnServe, { title: "story-focus-composer" });
  const row = await firstSidebarRow(page, serve.baseUrl);

  await row.click();
  await waitForStructuredView(page);
  await expect(composer(page)).toBeFocused({ timeout: 10_000 });

  // Outlast the mount-autofocus reclaim timers (250ms / 700ms) before blurring.
  await page.waitForTimeout(1_000);
  await blurActive(page);
  await expect(composer(page)).not.toBeFocused();

  await row.click();
  await expect(composer(page)).toBeFocused({ timeout: 10_000 });
});

test("command palette switches sessions", async ({ page, spawnServe }) => {
  const serve = await spawnServe({
    seedFn: ({ home, env }) => {
      for (const [title, subdir] of [
        ["palette-source", "project-a"],
        ["palette-target", "project-b"],
      ]) {
        const projectDir = initWorkingRepo(join(home, subdir!), env).path;
        const res = spawnSync(resolveAoeBinary(), ["add", projectDir, "-t", title!, "-c", "claude"], { env });
        if (res.status !== 0) {
          throw new Error(`aoe add ${title} failed: status=${res.status} stderr=${res.stderr?.toString() ?? "<none>"}`);
        }
      }
    },
  });
  const sessions = await listSessions(serve.baseUrl);
  const idOf = (title: string) => sessions.find((s) => s.title === title)!.id;

  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(idOf("palette-source"))}`);
  await expect(page).toHaveURL(new RegExp(`/session/${idOf("palette-source")}`), { timeout: 10_000 });

  await page.keyboard.press(`${MOD}+K`);
  const palette = page.getByRole("dialog", { name: "Command palette" });
  await expect(palette).toBeVisible({ timeout: 5_000 });
  await palette.getByPlaceholder("Search actions, sessions, settings…").fill("palette-target");
  await palette.getByText("palette-target").first().click();
  await expect(page).toHaveURL(new RegExp(`/session/${idOf("palette-target")}`), { timeout: 10_000 });
});

test("Cmd/Ctrl+` activates the paired terminal panel", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "story-terminal-focus" }) });
  const [seeded] = await listSessions(serve.baseUrl);
  await page.goto(`${serve.baseUrl}/session/${encodeURIComponent(seeded!.id)}`);
  await expect(page.locator('[data-testid="content-split-resize-handle"]')).toBeVisible({ timeout: 10_000 });

  // The chord activates the paired terminal tab and focuses it once its PTY is ready.
  await page.locator("body").click({ position: { x: 5, y: 5 } });
  await page.keyboard.press(`${MOD}+Backquote`);
  await expect(page.locator('[data-term="paired"]').first()).toBeVisible({ timeout: 15_000 });
  await expect(page.getByText(/Reconnecting/i)).toBeHidden({ timeout: 15_000 });
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const el = document.querySelector('[data-term="paired"]');
          return !!el && !!document.activeElement && el.contains(document.activeElement);
        }),
      { timeout: 10_000 },
    )
    .toBe(true);
});
