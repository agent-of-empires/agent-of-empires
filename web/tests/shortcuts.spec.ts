// Keyboard-shortcut stories ported from the live suite (#1419 era
// acp-stories): Cmd/Ctrl+B toggles the workspace sidebar, Shift+D
// toggles the diff pane specifically, and Cmd/Ctrl+Alt+B collapses or
// restores the whole right dock via the chord binding. All flip App.tsx
// state through useKeyboardShortcuts; the chords bind on e.code === "KeyB"
// so Mac layouts where Option+B emits "∫" still match.
//
// These need a mounted session view. The whole-dock toggle is observed via
// ContentSplit's drag handle (data-testid="content-split-resize-handle",
// present only when a session is open and the right dock has panes); the
// per-pane diff toggle is observed via its activity-bar button's aria-pressed.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { installSidebarMocks, threeSessionsInOneRepo } from "./helpers/sidebarMocks";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const SESSION = "pinch-test";
const PANE_LAYOUT_KEY = "aoe-pane-layout-v2";

async function rightDockState(page: Page): Promise<{ tabs: string[]; collapsed: boolean } | null> {
  return page.evaluate(
    ({ key, session }) => {
      const raw = localStorage.getItem(key);
      if (!raw) return null;
      const store = JSON.parse(raw) as {
        template?: { right?: { tabs?: string[] }[]; collapsed?: { right?: boolean } };
        sessions?: Record<string, { right?: { tabs?: string[] }[]; collapsed?: { right?: boolean } }>;
      };
      const layout = store.sessions?.[session] ?? store.template;
      if (!layout) return null;
      return {
        tabs: (layout.right ?? []).flatMap((g) => g.tabs ?? []),
        collapsed: layout.collapsed?.right === true,
      };
    },
    { key: PANE_LAYOUT_KEY, session: SESSION },
  );
}

// Force focus onto <body> before pressing a single-key shortcut.
// xterm.js's helper textarea steals focus when the terminal mounts or
// re-layouts; a focused textarea makes the input-gated shortcuts
// (Shift+D) no-ops and turns the keystroke into PTY bytes instead.
// The blur runs INSIDE the poll: a single early blur loses to xterm's
// asynchronous refocus on a slow runner, so re-blur on every attempt
// until body actually holds focus.
async function blurToBody(page: Page) {
  // xterm autofocuses its textarea when the WS connects (async, after
  // mount), so a blur that runs before that one-shot lands gets undone
  // and the shortcut keystroke types into the terminal instead. Wait
  // out the autofocus (soft timeout: it may have fired already), THEN
  // blur until body holds focus.
  const deadline = Date.now() + 1_500;
  while (Date.now() < deadline) {
    const tag = await page.evaluate(() => document.activeElement?.tagName ?? null);
    if (tag === "TEXTAREA") break;
    await page.waitForTimeout(50);
  }
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

test("Shift+D toggles the diff pane on a session view", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto(`/session/${SESSION}`);

  // Shift+D toggles the diff pane specifically (not the whole dock); the
  // activity-bar toggle's pressed state reflects whether diff is open.
  const diffToggle = page.locator('[data-testid="pane-toggle-diff"]');
  await expect(diffToggle).toHaveAttribute("aria-pressed", "true");

  await blurToBody(page);
  await page.keyboard.press("Shift+D");
  await expect(diffToggle).toHaveAttribute("aria-pressed", "false");

  // Re-blur: collapsing re-layouts the split, which can hand focus back to
  // the terminal via xterm's ResizeObserver focus-restore.
  await blurToBody(page);
  await page.keyboard.press("Shift+D");
  await expect(diffToggle).toHaveAttribute("aria-pressed", "true");
});

test("Cmd/Ctrl+Alt+B toggles the right panel on a session view", async ({ page }) => {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/session/pinch-test");

  const handle = page.locator('[data-testid="content-split-resize-handle"]');
  await expect(handle).toBeVisible();
  await expect.poll(() => rightDockState(page)).toEqual({ tabs: ["diff", "terminal:0"], collapsed: false });

  await page.keyboard.press("ControlOrMeta+Alt+b");
  await expect(handle).toBeHidden();
  await expect.poll(() => rightDockState(page)).toEqual({ tabs: ["diff", "terminal:0"], collapsed: true });

  await page.keyboard.press("ControlOrMeta+Alt+b");
  await expect(handle).toBeVisible();
  await expect.poll(() => rightDockState(page)).toEqual({ tabs: ["diff", "terminal:0"], collapsed: false });
});
