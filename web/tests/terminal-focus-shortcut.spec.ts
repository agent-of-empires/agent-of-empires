import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { mkdirSync } from "node:fs";

const SHOTS_DIR = "../target/focus-shortcut-screenshots";
mkdirSync(SHOTS_DIR, { recursive: true });
async function shot(page: Page, name: string) {
  await page.screenshot({ path: `${SHOTS_DIR}/${name}` });
}

async function openSession(page: Page) {
  await clickSidebarSession(page, "pinch-test");
  await expect(page.locator('[data-term="agent"]')).toHaveCount(1);
  await expect(page.locator('[data-term="agent"] .xterm')).toBeVisible();
}

async function focusedKind(page: Page): Promise<"agent" | "paired" | null> {
  return page.evaluate(() => {
    const active = document.activeElement;
    if (!active) return null;
    if (document.querySelector('[data-term="agent"]')?.contains(active)) {
      return "agent";
    }
    const paired = document.querySelectorAll('[data-term="paired"]');
    for (const p of paired) {
      if (p.contains(active)) return "paired";
    }
    return null;
  });
}

async function focusKind(page: Page, kind: "agent" | "paired") {
  const target = kind === "paired" ? "paired-visible" : "agent";
  if (target === "agent") {
    await page
      .locator('[data-term="agent"]')
      .first()
      .locator("textarea")
      .focus();
    return;
  }
  // The paired panel renders once (the inline desktop split copy); on
  // mobile it is promoted into the single pane. Filter by visibility to
  // hit whichever instance the user would actually use.
  const visiblePaired = page.locator('[data-term="paired"]:visible').first();
  await visiblePaired.locator("textarea").focus();
}

// Push focus off any panel so we can test the "from outside" behavior.
async function blurAll(page: Page) {
  await page.evaluate(() => {
    const a = document.activeElement as HTMLElement | null;
    a?.blur?.();
    document.body.focus();
  });
}

// ────────────────────────────────────────────────────────────────────
//  Desktop scenarios
// ────────────────────────────────────────────────────────────────────
test.describe("Cmd/Ctrl+` desktop", () => {
  test.use({ viewport: { width: 1280, height: 800 }, hasTouch: false });

  test("toggles between agent and paired with the right panel open", async ({
    page,
  }, _testInfo) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);

    await focusKind(page, "agent");
    await expect.poll(() => focusedKind(page)).toBe("agent");
    await shot(page, "01-agent-focused.png");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");
    await shot(page, "02-paired-focused.png");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");
    await shot(page, "03-back-to-agent.png");
  });

  test("first press from outside any terminal lands in paired (VSCode-like)", async ({
    page,
  }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);
    await blurAll(page);
    await expect.poll(() => focusedKind(page)).toBe(null);

    // Semantic match for VSCode's Ctrl+` "open/focus the terminal": from
    // outside both panes, focus lands in paired (the secondary shell).
    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });

  test("expands collapsed right panel and focuses paired (latch)", async ({
    page,
  }, _testInfo) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    await page.keyboard.press("ControlOrMeta+Alt+b");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(0);
    await shot(page, "04-collapsed.png");

    await focusKind(page, "agent");
    await page.keyboard.press("ControlOrMeta+`");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
    await expect.poll(() => focusedKind(page)).toBe("paired");
    await shot(page, "05-expanded-paired-focused.png");
  });

  test("paired latch fires once ensureTerminal resolves (slow paired)", async ({
    page,
  }) => {
    await mockTerminalApis(page);
    // Override the host-shell ensure with a 1500ms delay BEFORE goto.
    // Routes are matched in reverse registration order, so this wins over
    // the wildcard inside mockTerminalApis.
    await page.route("**/api/sessions/*/terminal", async (r) => {
      await new Promise((res) => setTimeout(res, 1500));
      await r.fulfill({ status: 200, body: "" });
    });

    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");
    await expect(page.locator('[data-term="agent"]')).toHaveCount(1);

    // Press Cmd+` immediately while paired is still in its "Starting…"
    // state. focusSelf in PairedTerminal can't find a textarea, so the
    // listener calls setPendingTerminalFocus("paired").
    await focusKind(page, "agent");
    await page.keyboard.press("ControlOrMeta+`");

    // Within 3s the ensureTerminal mock returns, ready flips true,
    // the consume-on-ready effect fires, focus lands in paired.
    await expect
      .poll(() => focusedKind(page), { timeout: 3000 })
      .toBe("paired");
  });

  test("agent latch fires once ensureSession resolves (slow agent)", async ({
    page,
  }) => {
    await mockTerminalApis(page);
    await page.route("**/api/sessions/*/ensure", async (r) => {
      await new Promise((res) => setTimeout(res, 1500));
      await r.fulfill({ json: { ok: true } });
    });

    await page.goto("/");
    await clickSidebarSession(page, "pinch-test");

    // Wait for paired to be ready (its ensureTerminal isn't delayed); use
    // it as the focus source so target=agent.
    const paired = page.locator('[data-term="paired"]:visible').first();
    await expect(paired.locator(".xterm")).toBeVisible();
    await paired.locator("textarea").focus();
    await expect.poll(() => focusedKind(page)).toBe("paired");

    // Agent terminal still mounted as "Starting session..." so its xterm
    // textarea doesn't exist yet. Press Cmd+` → target=agent → listener
    // sets the pending latch.
    await page.keyboard.press("ControlOrMeta+`");

    // ensureSession resolves, ensureState flips to ready, consume effect
    // fires, focus lands on agent.
    await expect.poll(() => focusedKind(page), { timeout: 3000 }).toBe("agent");
  });

  test("with diff viewer open, Cmd+` to agent closes the diff", async ({
    page,
  }, _testInfo) => {
    await mockTerminalApis(page);
    // Provide one file in the diff list. Don't mock the file content
    // endpoint — DiffFileViewer can render an error state and the test
    // only cares about selectedFilePath being set (which hides the agent
    // wrapper).
    await page.route("**/api/sessions/*/diff/files", (r) =>
      r.fulfill({
        json: {
          files: [
            {
              path: "src/foo.ts",
              old_path: null,
              status: "modified",
              additions: 3,
              deletions: 1,
            },
          ],
          per_repo_bases: [{ base_branch: "main" }],
          warning: null,
        },
      }),
    );

    await page.goto("/");
    await openSession(page);

    // Click the file in the diff list.
    await page.locator('button:has-text("foo.ts")').first().click();
    // The agent terminal wrapper is now className="hidden". The
    // [data-term="agent"] node still exists but inside a hidden parent.
    await shot(page, "06-diff-open.png");

    await focusKind(page, "paired");
    await expect.poll(() => focusedKind(page)).toBe("paired");

    // Press Cmd+` → handler clears selectedFilePath, then rAF-dispatches.
    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");
    await shot(page, "07-after-toggle-agent.png");
  });

  test("rapid repeated presses end in a stable state", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);
    await focusKind(page, "agent");

    // 11 toggles total = odd flips from agent → paired.
    for (let i = 0; i < 11; i++) {
      await page.keyboard.press("ControlOrMeta+`");
    }
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });

  test("term-focused CSS class follows the focused panel", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    await focusKind(page, "agent");
    await expect(page.locator('[data-term="agent"]').first()).toHaveClass(
      /term-focused/,
    );

    await page.keyboard.press("ControlOrMeta+`");
    // The visible paired panel should pick up term-focused.
    await expect(
      page.locator('[data-term="paired"]:visible').first(),
    ).toHaveClass(/term-focused/);
  });
});

// ────────────────────────────────────────────────────────────────────
//  Mobile scenario — proves the data-term fix
// ────────────────────────────────────────────────────────────────────
test.describe("Cmd/Ctrl+` mobile", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true });

  test("toggle works correctly on a mobile viewport", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    // Sidebar is collapsed on mobile by default; open it to access the
    // session list.
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await openSession(page);

    // Single full-viewport pane on mobile (#1452). Cmd+` promotes and
    // focuses the paired shell, mounting it lazily; there is exactly one
    // paired instance, no slide-in copy.
    await page.keyboard.press("ControlOrMeta+`");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
    await expect.poll(() => focusedKind(page)).toBe("paired");

    // Pressing again returns to the agent terminal.
    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });
});

// ────────────────────────────────────────────────────────────────────
//  Help overlay documents the shortcut
// ────────────────────────────────────────────────────────────────────
test.describe("Help overlay", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("lists the new Cmd+` shortcut row", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");

    // Open the help overlay via the TopBar "More options" menu rather than
    // pressing "?" — synthetic key events for `?` proved finicky across
    // keyboard layouts and the menu path is what users actually use.
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Help" }).click();

    const row = page.getByText("Toggle agent / shell terminal focus");
    await expect(row).toBeVisible();
  });
});
