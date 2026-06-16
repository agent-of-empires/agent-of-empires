// User story (#1643): a user who lives in the command palette can start a
// scratch session. Open Cmd+K / Ctrl+K, run "New scratch session", and the
// wizard opens prefilled for a scratch session jumped to the Review step;
// Cmd+Enter / Ctrl+Enter then launches it. Read-only mode hides the creation
// commands from the palette entirely.

import { basename, dirname } from "node:path";
import { test, expect } from "../helpers/liveTest";
import { listSessions } from "../helpers/aoeServe";

const PALETTE_PLACEHOLDER = "Search actions, sessions, settings…";

test("palette 'New scratch session' opens the wizard and launches a scratch session", async ({ serve, page }) => {
  await page.goto(serve.baseUrl);
  await expect(page.getByRole("button", { name: "New session", exact: true }).first()).toBeVisible({ timeout: 15_000 });

  // ControlOrMeta honors the host's actual modifier (Cmd on macOS, Ctrl on
  // CI), matching how useKeyboardShortcuts derives the palette chord.
  await page.keyboard.press("ControlOrMeta+KeyK");
  await expect(page.getByPlaceholder(PALETTE_PLACEHOLDER)).toBeVisible();

  await page.getByPlaceholder(PALETTE_PLACEHOLDER).fill("scratch");
  await page.getByRole("option", { name: /New scratch session/i }).click();

  const wizard = page.locator('div.fixed.inset-0.z-50:has(h1:has-text("New session"))');
  await expect(wizard).toBeVisible({ timeout: 10_000 });

  // The palette command opens the single-screen wizard with scratch enabled;
  // the Launch button + scratch callout prove the prefill plumbing fired.
  await expect(wizard.getByRole("button", { name: /Launch session/ })).toBeVisible({ timeout: 10_000 });
  await expect(wizard.getByText("Scratch session")).toBeVisible();

  await page.keyboard.press("ControlOrMeta+Enter");

  await expect
    .poll(async () => (await listSessions(serve.baseUrl)).length, {
      timeout: 15_000,
    })
    .toBeGreaterThan(0);

  const sessions = await listSessions(serve.baseUrl);
  expect(sessions).toHaveLength(1);
  const session = sessions[0]!;
  expect(session.scratch).toBe(true);
  const projectPath = session.project_path as string;
  expect(basename(dirname(projectPath))).toBe("scratch");
});

test("palette hides creation commands in read-only mode", async ({ serveReadOnly, page }) => {
  // Wait for /api/about so the React state carries read_only before the
  // palette renders; otherwise serverAbout is null and the guard is bypassed.
  const aboutPromise = page.waitForResponse((r) => r.url().endsWith("/api/about") && r.status() === 200, {
    timeout: 10_000,
  });
  await page.goto(serveReadOnly.baseUrl);
  await aboutPromise;
  await page.waitForTimeout(200);

  await page.locator("body").click();
  await page.keyboard.press("ControlOrMeta+KeyK");
  await expect(page.getByPlaceholder(PALETTE_PLACEHOLDER)).toBeVisible();

  await expect(page.getByRole("option", { name: /New scratch session/i })).toHaveCount(0);
  // Exclude per-setting palette entries (#2108): the "New Session Attach Mode"
  // setting also matches /New session/i but is a settings jump, not a creation
  // command. Its row carries the "Opens settings" subtitle; creation commands
  // do not.
  await expect(page.getByRole("option", { name: /New session/i }).filter({ hasNotText: "Opens settings" })).toHaveCount(
    0,
  );
});
