import { makePatch } from "./helpers/patch";
import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { clickSidebarSession } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

async function openSession(page: Page) {
  await clickSidebarSession(page, "pinch-test");
  await expect(page.locator('[data-term="agent"]')).toHaveCount(1);
  await expect(
    page.locator('[data-term="agent"] .xterm, [data-term="agent"] [data-live-terminal]').first(),
  ).toBeVisible();
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
  if (kind === "agent") {
    await page.locator('[data-term="agent"]').first().locator("textarea").focus();
    return;
  }
  // #2437: on desktop the paired shell mounts only while its tab is active.
  const termTab = page.locator('[data-testid^="pane-tab-terminal:"]').first();
  if (await termTab.count()) await termTab.click();
  const visiblePaired = page.locator('[data-term="paired"]:visible').first();
  await visiblePaired.locator("textarea").focus();
}

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

  test("toggles between agent and paired with the right panel open, stably under rapid presses", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);
    await blurAll(page);
    await expect.poll(() => focusedKind(page)).toBe(null);

    // Like VS Code, from outside both panes focus goes to the paired shell.
    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");

    await focusKind(page, "agent");
    await expect.poll(() => focusedKind(page)).toBe("agent");
    for (let i = 0; i < 11; i++) {
      await page.keyboard.press("ControlOrMeta+`");
    }
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });

  test("expands collapsed right panel and focuses paired (latch)", async ({ page }) => {
    await mockTerminalApis(page);
    await page.goto("/");
    await openSession(page);

    await page.keyboard.press("ControlOrMeta+Alt+b");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(0);

    await focusKind(page, "agent");
    await page.keyboard.press("ControlOrMeta+`");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });

  test("paired latch fires once ensureTerminal resolves (slow paired)", async ({ page }) => {
    await mockTerminalApis(page);
    let releaseTerminal!: () => void;
    const terminalPending = new Promise<void>((resolve) => {
      releaseTerminal = resolve;
    });
    let requested = false;
    await page.route("**/api/sessions/*/terminal*", async (route) => {
      if (route.request().method() !== "POST") return route.fallback();
      requested = true;
      await terminalPending;
      await route.fulfill({ status: 200, body: "" });
    });

    try {
      await page.goto("/");
      await openSession(page);
      await page.locator('[data-testid^="pane-tab-terminal:"]').first().click();
      await expect.poll(() => requested).toBe(true);
      await focusKind(page, "agent");
      await expect.poll(() => focusedKind(page)).toBe("agent");
      await expect(page.locator('[data-term="paired"] textarea')).toHaveCount(0);
      await page.keyboard.press("ControlOrMeta+`");
    } finally {
      releaseTerminal();
    }
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });

  test("with diff viewer open, Cmd+` to agent closes the diff", async ({ page }) => {
    await mockTerminalApis(page);
    const file = { path: "src/foo.ts", old_path: null, status: "modified", additions: 1, deletions: 1 };
    const oldContent = "export const value = 1;\n";
    const newContent = "export const value = 2;\n";
    await page.route("**/api/sessions/*/diff/files", (route) =>
      route.fulfill({
        json: { files: [file], per_repo_bases: [{ base_branch: "main" }], warning: null },
      }),
    );
    await page.route(/\/api\/sessions\/[^/]+\/diff\/file\?/, (route) =>
      route.fulfill({
        json: {
          file,
          old_content: oldContent,
          new_content: newContent,
          is_binary: false,
          truncated: false,
          patch: makePatch(file.path, oldContent, newContent),
        },
      }),
    );

    await page.goto("/");
    await openSession(page);

    await page.locator('button:has-text("foo.ts")').first().click();
    const agent = page.locator('[data-term="agent"]');
    const backToTerminal = page.getByRole("button", { name: "Back to terminal" });
    await expect(backToTerminal).toBeVisible();
    await expect(agent).toHaveCount(1);
    await expect(agent).toBeHidden();

    await focusKind(page, "paired");
    await expect.poll(() => focusedKind(page)).toBe("paired");
    await expect(backToTerminal).toBeVisible();
    await expect(agent).toBeHidden();

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");
    await expect(agent).toBeVisible();
    await expect(backToTerminal).toBeHidden();
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
    await page.getByRole("button", { name: "Toggle sidebar" }).click();
    await openSession(page);

    // #1452: one full-viewport pane; the chord promotes a single paired instance.
    await page.keyboard.press("ControlOrMeta+`");
    await expect(page.locator('[data-term="paired"]')).toHaveCount(1);
    await expect.poll(() => focusedKind(page)).toBe("paired");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("agent");

    await page.keyboard.press("ControlOrMeta+`");
    await expect.poll(() => focusedKind(page)).toBe("paired");
  });
});
