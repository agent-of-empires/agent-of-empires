// Moving between sessions: sidebar select focus, command palette, terminal chord, wizard launch.

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

test("coarse pointer: selecting a structured view session focuses the composer (auto-open keyboard)", async ({
  page,
  spawnServe,
}) => {
  // Force a touch-only profile; mount autofocus is suppressed there, so only auto-open keyboard can focus.
  await page.addInitScript(() => {
    const orig = window.matchMedia.bind(window);
    const forced: Record<string, boolean> = { "(pointer: coarse)": true, "(any-pointer: fine)": false };
    window.matchMedia = (query: string) =>
      query in forced
        ? ({
            matches: forced[query],
            media: query,
            onchange: null,
            addEventListener: () => {},
            removeEventListener: () => {},
            addListener: () => {},
            removeListener: () => {},
            dispatchEvent: () => false,
          } as MediaQueryList)
        : orig(query);
  });
  const { serve } = await startAcpSession(spawnServe, { title: "story-focus-composer-coarse" });
  const row = await firstSidebarRow(page, serve.baseUrl);

  await row.click();
  await waitForStructuredView(page);
  await expect(composer(page)).toBeFocused({ timeout: 10_000 });
});

test("desktop: re-selecting the active terminal session refocuses the textarea", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "story-focus-term" }) });
  const [seeded] = await listSessions(serve.baseUrl);
  const liveInputFocused = () =>
    page.evaluate(() => !!document.activeElement?.matches('textarea[aria-label="Live terminal input"]'));
  const row = await firstSidebarRow(page, serve.baseUrl);

  await row.click();
  await expect(page).toHaveURL(new URL(`/session/${encodeURIComponent(seeded!.id)}`, serve.baseUrl).toString(), {
    timeout: 10_000,
  });
  await expect.poll(liveInputFocused, { timeout: 10_000 }).toBe(true);

  await blurActive(page);
  await expect.poll(liveInputFocused).toBe(false);
  await row.click();
  await expect.poll(liveInputFocused, { timeout: 10_000 }).toBe(true);
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

test("Launch button on Review step creates the session", async ({ page, spawnServe }) => {
  const serve = await spawnServe({ seedFn: seedSessionViaAoeAdd({ title: "story-wizard-launch-button-seed" }) });
  await page.goto(serve.baseUrl);
  await page
    .locator('[data-testid="sidebar-group-header"]')
    .first()
    .getByRole("button", { name: /New session in /i })
    .click();

  const wizard = page.getByTestId("session-wizard");
  await expect(wizard).toBeVisible({ timeout: 10_000 });
  await wizard.getByPlaceholder("Auto-generated if empty").fill("story-launched-button");
  const before = await listSessions(serve.baseUrl);
  await wizard.getByRole("button", { name: /Launch session/i }).click();

  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).length, { timeout: 20_000 })
    .toBeGreaterThan(before.length);
  await expect(
    page.locator('[data-testid="sidebar-session-row"]').filter({ hasText: "story-launched-button" }),
  ).toHaveCount(1, { timeout: 15_000 });
});
