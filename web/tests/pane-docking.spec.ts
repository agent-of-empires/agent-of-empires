// Dockable panes: tab strips, activity-bar toggles, moves between docks, split groups, and plugin panes.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { mockTerminalApis } from "./helpers/terminal-mocks";
import { openPanelsByDefault } from "./helpers/apiMocks";

const SESSION = "pinch-test";

async function openSession(page: Page) {
  await mockTerminalApis(page);
  await openPanelsByDefault(page);
  await page.setViewportSize({ width: 1280, height: 720 });
}

/** The terminal opens in the right dock, in its own group below Diff, and the bottom dock stays empty. */
async function terminalBelowDiff(page: Page) {
  await expect(page.getByTestId("pane-tab-terminal:pinch-test:0")).toBeVisible();
  await expect.poll(() => dockGroupCount(page, "right")).toBe(2);
  await expect(page.getByTestId("bottom-dock-resize")).toHaveCount(0);
}

async function openFromPaneMenu(page: Page, id: string) {
  await page.getByTestId("pane-menu-toggle").click();
  await page.getByTestId(`pane-toggle-${id}`).click();
  await expect(page.getByTestId(`pane-tab-${id}`)).toBeVisible();
}

async function dockTabOrder(page: Page, dock: "right" | "bottom"): Promise<string[]> {
  return page.$$eval(`[data-pane-dock="${dock}"] [data-testid^="pane-tab-"]`, (els) =>
    els.map((el) => (el.getAttribute("data-testid") ?? "").replace("pane-tab-", "")),
  );
}

async function dockGroupCount(page: Page, dock: "right" | "bottom"): Promise<number> {
  return page.locator(`[data-pane-dock="${dock}"]`).count();
}

/** Drag a tab past the 8px MouseSensor threshold, run `mid` while held, then drop on `target`. */
async function dragTab(page: Page, fromId: string, target: { x: number; y: number }, mid?: () => Promise<void>) {
  const from = await page.getByTestId(`pane-tab-${fromId}`).boundingBox();
  if (!from) throw new Error(`missing tab ${fromId}`);
  await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
  await page.mouse.down();
  await page.mouse.move(from.x + from.width / 2 + 12, from.y + from.height / 2, { steps: 4 });
  await page.mouse.move(target.x, target.y, { steps: 12 });
  if (mid) await mid();
  await page.mouse.up();
}

async function tabCenter(page: Page, id: string): Promise<{ x: number; y: number }> {
  const b = await page.getByTestId(`pane-tab-${id}`).boundingBox();
  if (!b) throw new Error(`missing tab ${id}`);
  return { x: b.x + b.width / 2, y: b.y + b.height / 2 };
}

test.describe("Dockable pane system", () => {
  test("the header toggles the review panel and terminal, the pane menu toggles panes, and the new-terminal button adds a closable tab", async ({
    page,
  }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    const panelToggle = page.getByTestId("right-dock-toggle");
    const termToggle = page.getByTestId("pane-toggle-terminal");
    await expect(panelToggle).toHaveAttribute("aria-pressed", "true");
    await expect(termToggle).toHaveAttribute("aria-pressed", "true");

    // The pane menu checks open panes and toggles them independently of the terminal.
    await page.getByTestId("pane-menu-toggle").click();
    await expect(page.getByTestId("pane-toggle-diff")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("pane-toggle-files")).toHaveAttribute("aria-checked", "false");
    await expect(page.getByTestId("pane-toggle-diff")).toBeFocused();
    await page.keyboard.press("ArrowDown");
    await expect(page.getByTestId("pane-toggle-files")).toBeFocused();
    await page.getByTestId("pane-toggle-files").click();
    await expect(page.getByTestId("pane-tab-files")).toBeVisible();
    await page.getByTestId("pane-menu-toggle").click();
    await page.getByTestId("pane-toggle-diff").click();
    await expect(page.getByTestId("pane-tab-diff")).toHaveCount(0);
    await expect(termToggle).toHaveAttribute("aria-pressed", "true");

    // The header toggle hides and restores the whole review panel, keeping its panes.
    await panelToggle.click();
    await expect(panelToggle).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("pane-tab-files")).toHaveCount(0);
    await panelToggle.click();
    await expect(page.getByTestId("pane-tab-files")).toBeVisible();

    // The new-terminal button opens a second terminal tab that can be closed.
    await expect(page.getByTestId("pane-tab-terminal:pinch-test:1")).toHaveCount(0);
    await page.getByLabel("New terminal for pinch-test").first().click();
    await expect(page.getByTestId("pane-tab-terminal:pinch-test:1")).toBeVisible();
    await page.getByLabel("Close pinch-test · 2").click();
    await expect(page.getByTestId("pane-tab-terminal:pinch-test:1")).toHaveCount(0);
    await expect(page.getByTestId("pane-tab-terminal:pinch-test:0")).toBeVisible();
  });

  test("a pane moves from the right dock to the bottom dock", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);
    await terminalBelowDiff(page);

    await page.getByTestId("pane-tab-diff").click();
    await page.getByLabel("Move diff to bottom dock").click();

    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    await expect(page.getByLabel("Move diff to right dock")).toBeVisible();
    expect(await dockTabOrder(page, "bottom")).toEqual(["diff"]);
  });

  test("a plugin pane renders as a dockable tool-window, its action hits the worker, and it survives a dock collapse", async ({
    page,
  }) => {
    await openSession(page);

    await page.route("**/api/plugins/ui-state", (route) =>
      route.fulfill({
        json: {
          entries: [
            {
              plugin_id: "acme.demo",
              slot: "pane",
              id: "demo_pane",
              session_id: SESSION,
              payload: {
                title: "Demo",
                default_location: "right",
                blocks: [
                  { kind: "heading", text: "Demo" },
                  { kind: "action", label: "Reload", method: "demo.reload" },
                ],
              },
            },
          ],
          notifications: [],
        },
      }),
    );

    let actionBody: { method?: string } | null = null;
    await page.route("**/api/plugins/acme.demo/action", async (route) => {
      actionBody = route.request().postDataJSON();
      await route.fulfill({ status: 202, json: { ok: true } });
    });

    await page.goto(`/session/${SESSION}`);

    // Plugin panes get a pane-menu entry but open only on demand (`autoOpenPluginPanes` defaults off).
    const paneId = "plugin:acme.demo:demo_pane";
    await page.getByTestId("pane-menu-toggle").click();
    await expect(page.locator(`[data-testid="pane-toggle-${paneId}"]`)).toBeVisible();
    await expect(page.getByTestId(`pane-tab-${paneId}`)).toHaveCount(0);
    await page.locator(`[data-testid="pane-toggle-${paneId}"]`).click();
    await expect(page.getByTestId(`pane-tab-${paneId}`)).toBeVisible();
    await expect(page.locator('[data-testid="plugin-pane-body"][data-plugin-id="acme.demo"]')).toBeVisible();

    await page.getByTestId("plugin-pane-action").click();
    await expect.poll(() => actionBody?.method).toBe("demo.reload");

    // Collapsing and re-expanding the right dock keeps the active plugin pane.
    const handle = page.getByTestId("content-split-resize-handle");
    await page.keyboard.press("ControlOrMeta+Alt+b");
    await expect(handle).toBeHidden();
    await page.keyboard.press("ControlOrMeta+Alt+b");
    await expect(handle).toBeVisible();
    await expect(page.getByTestId(`pane-tab-${paneId}`)).toBeVisible();
    await expect(page.locator('[data-testid="plugin-pane-body"][data-plugin-id="acme.demo"]')).toBeVisible();
  });

  test("dragging a tab reorders it within a dock and the order persists across reload", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);
    await terminalBelowDiff(page);
    await openFromPaneMenu(page, "files");
    expect(await dockTabOrder(page, "right")).toEqual(["diff", "files", "terminal:pinch-test:0"]);

    const diff = await page.getByTestId("pane-tab-diff").boundingBox();
    if (!diff) throw new Error("missing diff tab");
    await dragTab(page, "files", { x: diff.x + 4, y: diff.y + diff.height / 2 });
    await expect.poll(() => dockTabOrder(page, "right")).toEqual(["files", "diff", "terminal:pinch-test:0"]);
    await expect
      .poll(() =>
        page.evaluate((session) => {
          const saved = JSON.parse(localStorage.getItem("aoe-pane-layout-v4") ?? "null") as {
            sessions?: Record<string, { right: { tabs: string[] }[] }>;
          } | null;
          return saved?.sessions?.[session]?.right.flatMap((group) => group.tabs);
        }, SESSION),
      )
      .toEqual(["files", "diff", "terminal:pinch-test:0"]);

    await page.reload();
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect.poll(() => dockTabOrder(page, "right")).toEqual(["files", "diff", "terminal:pinch-test:0"]);
  });

  test("dragging a tab onto the empty bottom dock opens it there", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);
    await terminalBelowDiff(page);
    await expect(page.getByTestId("bottom-dock-resize")).toHaveCount(0);

    await dragTab(page, "diff", { x: 640, y: 715 }, async () => {
      const zone = page.getByTestId("empty-dock-dropzone-bottom");
      await expect(zone).toBeVisible();
      const box = await zone.boundingBox();
      if (box) await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 4 });
    });

    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    await expect.poll(() => dockTabOrder(page, "bottom")).toEqual(["diff"]);
    expect(await dockTabOrder(page, "right")).toEqual(["terminal:pinch-test:0"]);
  });

  test("a cross-dock drag shows an insertion marker and moves the pane", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);
    await terminalBelowDiff(page);

    await page.getByTestId("pane-tab-diff").click();
    await page.getByLabel("Move diff to bottom dock").click();
    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    expect(await dockTabOrder(page, "right")).toEqual(["terminal:pinch-test:0"]);
    expect(await dockTabOrder(page, "bottom")).toEqual(["diff"]);

    // Cross-dock inserts show a marker because the destination strip does not shift.
    const diff = await tabCenter(page, "diff");
    await dragTab(page, "terminal:pinch-test:0", diff, async () => {
      await expect(page.getByTestId("pane-insertion-marker")).toBeVisible();
    });

    await expect.poll(() => dockTabOrder(page, "right")).toEqual([]);
    await expect.poll(() => dockTabOrder(page, "bottom")).toContain("terminal:pinch-test:0");
  });

  test("dragging a tab onto a pane body splits the right dock into two groups that persist", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    await terminalBelowDiff(page);
    await openFromPaneMenu(page, "files");

    // Dropping on the trailing split half creates another group instead of reordering.
    await dragTab(page, "files", { x: 1000, y: 200 }, async () => {
      const zone = page.getByTestId("pane-split-right-0-after");
      await expect(zone).toBeVisible();
      const box = await zone.boundingBox();
      if (box) await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 6 });
    });

    await expect.poll(() => dockGroupCount(page, "right")).toBe(3);
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect(page.getByTestId("pane-tab-files")).toBeVisible();

    // The split layout round-trips through localStorage.
    await page.reload();
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect.poll(() => dockGroupCount(page, "right")).toBe(3);

    // Closing one group's pane prunes only that group; the others stay valid.
    await page.getByLabel("Close Files").click();
    await expect(page.getByTestId("pane-tab-files")).toHaveCount(0);
    await expect.poll(() => dockGroupCount(page, "right")).toBe(2);
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
  });
});
