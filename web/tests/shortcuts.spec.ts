// Keyboard-shortcut stories ported from the live suite (#1419 era
// acp-stories): Cmd/Ctrl+B toggles the workspace sidebar, Shift+D
// toggles the diff (right) panel, and Cmd/Ctrl+Alt+B toggles the
// right panel via the chord binding. All three flip App.tsx state
// through useKeyboardShortcuts; the chords bind on e.code === "KeyB"
// so Mac layouts where Option+B emits "∫" still match.
//
// The panel toggles need a mounted session view: ContentSplit only
// renders its drag handle (data-testid="content-split-resize-handle")
// when a session is open and the right pane is expanded, so the
// handle's presence is the user-visible signal.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";
import { mockTerminalApis } from "./helpers/terminal-mocks";

// Force focus onto <body> before pressing a single-key shortcut.
// xterm.js's helper textarea steals focus when the terminal mounts or
// re-layouts; a focused textarea makes the input-gated shortcuts
// (Shift+D) no-ops and turns the keystroke into PTY bytes instead.
// The blur runs INSIDE the poll: a single early blur loses to xterm's
// asynchronous refocus on a slow runner, so re-blur on every attempt
// until body actually holds focus.
async function blurToBody(page: Page) {
  await expect
    .poll(() =>
      page.evaluate(() => {
        const ae = document.activeElement as HTMLElement | null;
        if (ae && ae !== document.body) ae.blur?.();
        return document.activeElement?.tagName ?? null;
      }),
    )
    .toBe("BODY");
}

test("Cmd/Ctrl+B toggles the workspace sidebar", async ({ page }) => {
  await installSidebarMocks(page, { sessions: threeSessionsInOneRepo() });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");

  const sessionRow = page.locator('[data-testid="sidebar-session-row"]').first();
  await expect(sessionRow).toBeVisible();

  // Global chord: fires regardless of focus.
  await page.keyboard.press("ControlOrMeta+b");
  await expect(sessionRow).toBeHidden();

  await page.keyboard.press("ControlOrMeta+b");
  await expect(sessionRow).toBeVisible();
});

test("Shift+D toggles the diff panel on a session view", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/session/pinch-test");

  const handle = page.locator('[data-testid="content-split-resize-handle"]');
  await expect(handle).toBeVisible();

  await blurToBody(page);
  await page.keyboard.press("Shift+D");
  await expect(handle).toBeHidden();

  // Collapsing re-layouts the split, which can hand focus back to the
  // terminal via xterm's ResizeObserver focus-restore; re-blur so the
  // second press is a shortcut and not a literal "D" keystroke.
  await blurToBody(page);
  await page.keyboard.press("Shift+D");
  await expect(handle).toBeVisible();
});

test("Cmd/Ctrl+Alt+B toggles the right panel on a session view", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/session/pinch-test");

  const handle = page.locator('[data-testid="content-split-resize-handle"]');
  await expect(handle).toBeVisible();

  await page.keyboard.press("ControlOrMeta+Alt+b");
  await expect(handle).toBeHidden();

  await page.keyboard.press("ControlOrMeta+Alt+b");
  await expect(handle).toBeVisible();
});
