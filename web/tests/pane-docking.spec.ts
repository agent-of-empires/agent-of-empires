// The dockable pane system (JetBrains-style): docks render their panes as a
// tab strip, an activity-bar strip toggles each pane kind, panes move between
// the right and bottom docks, terminals open as multiple tabs, and a plugin's
// `pane` slot is a first-class tab with an action button that round-trips to
// the worker. Mocked (no daemon); the plugin UI-state poll is stubbed so the
// test owns the plugin entry it renders.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";
import { mockTerminalApis } from "./helpers/terminal-mocks";

const SESSION = "pinch-test";

async function openSession(page: Page) {
  await mockTerminalApis(page);
  await page.setViewportSize({ width: 1280, height: 720 });
}

/** Tab ids in a dock, in rendered (left-to-right) order. */
async function dockTabOrder(page: Page, dock: "right" | "bottom"): Promise<string[]> {
  return page.$$eval(`[data-pane-dock="${dock}"] [data-testid^="pane-tab-"]`, (els) =>
    els.map((el) => (el.getAttribute("data-testid") ?? "").replace("pane-tab-", "")),
  );
}

/** How many split groups a dock currently renders (one `[data-pane-dock]`
 *  section per group). */
async function dockGroupCount(page: Page, dock: "right" | "bottom"): Promise<number> {
  return page.locator(`[data-pane-dock="${dock}"]`).count();
}

/** Press on a tab's activation button (where the drag listeners live), move past
 *  the 8px MouseSensor threshold, run `mid` while held, then drop on `target`.
 *  Playwright's mouse maps to dnd-kit's MouseSensor, so no press-hold delay. */
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
  test("the activity bar toggles the built-in diff and terminal panes", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    const diffToggle = page.locator('[data-testid="pane-toggle-diff"]');
    const termToggle = page.locator('[data-testid="pane-toggle-terminal"]');
    await expect(diffToggle).toHaveAttribute("aria-pressed", "true");
    await expect(termToggle).toHaveAttribute("aria-pressed", "true");

    // Closing diff via its activity-bar icon hides the diff tool-window but
    // leaves terminal open (panes toggle independently).
    await diffToggle.click();
    await expect(diffToggle).toHaveAttribute("aria-pressed", "false");
    await expect(termToggle).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByLabel("Move diff to bottom dock")).toHaveCount(0);

    // Reopen it.
    await diffToggle.click();
    await expect(diffToggle).toHaveAttribute("aria-pressed", "true");
  });

  test("a pane moves from the right dock to the bottom dock", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    // No bottom dock until something is docked there.
    await expect(page.getByTestId("bottom-dock-resize")).toHaveCount(0);

    // The diff pane's frame carries a move-to-bottom control.
    await page.getByLabel("Move diff to bottom dock").click();

    // Bottom dock now exists, and diff's frame offers the reverse move.
    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    await expect(page.getByLabel("Move diff to right dock")).toBeVisible();
  });

  test("a plugin pane renders as a dockable tool-window and its action hits the worker", async ({ page }) => {
    await openSession(page);

    // Stub the plugin UI-state poll with one pane carrying an action button.
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

    // The plugin pane gets its own activity-bar toggle and a dock tab.
    const paneId = "plugin:acme.demo:demo_pane";
    await expect(page.locator(`[data-testid="pane-toggle-${paneId}"]`)).toBeVisible();
    // Only the active tab's body mounts, so activate the plugin tab first.
    await page.getByTestId(`pane-tab-${paneId}`).click();
    await expect(page.locator('[data-testid="plugin-pane-body"][data-plugin-id="acme.demo"]')).toBeVisible();

    // Clicking the pane's action button forwards its method to the worker.
    await page.getByTestId("plugin-pane-action").click();
    await expect.poll(() => actionBody?.method).toBe("demo.reload");
  });

  test("right-dock collapse shortcut preserves an active plugin pane", async ({ page }) => {
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
                blocks: [{ kind: "heading", text: "Demo" }],
              },
            },
          ],
          notifications: [],
        },
      }),
    );

    await page.goto(`/session/${SESSION}`);

    const paneId = "plugin:acme.demo:demo_pane";
    await page.getByTestId(`pane-tab-${paneId}`).click();
    await expect(page.locator('[data-testid="plugin-pane-body"][data-plugin-id="acme.demo"]')).toBeVisible();

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

    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();
    expect(await dockTabOrder(page, "right")).toEqual(["diff", "terminal:0"]);

    // Drag the terminal tab onto diff's left half so it lands first.
    const diff = await page.getByTestId("pane-tab-diff").boundingBox();
    if (!diff) throw new Error("missing diff tab");
    await dragTab(page, "terminal:0", { x: diff.x + 4, y: diff.y + diff.height / 2 });
    await expect.poll(() => dockTabOrder(page, "right")).toEqual(["terminal:0", "diff"]);

    // The reordered layout round-trips through localStorage.
    await page.reload();
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect.poll(() => dockTabOrder(page, "right")).toEqual(["terminal:0", "diff"]);
  });

  test("dragging a tab onto the empty bottom dock opens it there", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);
    await expect(page.getByTestId("bottom-dock-resize")).toHaveCount(0);

    // The empty-dock landing zone exists only while a pane tab is dragged.
    await dragTab(page, "diff", { x: 640, y: 715 }, async () => {
      const zone = page.getByTestId("empty-dock-dropzone-bottom");
      await expect(zone).toBeVisible();
      const box = await zone.boundingBox();
      if (box) await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 4 });
    });

    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    await expect.poll(() => dockTabOrder(page, "bottom")).toEqual(["diff"]);
    expect(await dockTabOrder(page, "right")).toEqual(["terminal:0"]);
  });

  test("a cross-dock drag shows an insertion marker and moves the pane", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    // Split the docks: the active diff tab moves to the bottom, terminal stays.
    await page.getByLabel("Move diff to bottom dock").click();
    await expect(page.getByTestId("bottom-dock-resize")).toBeVisible();
    expect(await dockTabOrder(page, "right")).toEqual(["terminal:0"]);
    expect(await dockTabOrder(page, "bottom")).toEqual(["diff"]);

    // Drag terminal onto the bottom dock's diff tab; the marker appears because
    // the destination strip does not shift to preview a cross-dock insert.
    const diff = await tabCenter(page, "diff");
    await dragTab(page, "terminal:0", diff, async () => {
      await expect(page.getByTestId("pane-insertion-marker")).toBeVisible();
    });

    await expect.poll(() => dockTabOrder(page, "right")).toEqual([]);
    await expect.poll(() => dockTabOrder(page, "bottom")).toContain("terminal:0");
  });

  test("the new-terminal button opens a second terminal tab that can be closed", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    // Default right dock has the primary terminal tab.
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();
    await expect(page.getByTestId("pane-tab-terminal:1")).toHaveCount(0);

    // The "+" in the strip allocates a fresh terminal instance.
    await page.getByLabel("New terminal").first().click();
    await expect(page.getByTestId("pane-tab-terminal:1")).toBeVisible();

    // Closing the extra terminal tab removes it; the primary one stays.
    await page.getByLabel("Close terminal 2").click();
    await expect(page.getByTestId("pane-tab-terminal:1")).toHaveCount(0);
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();
  });

  test("dragging a tab onto a pane body splits the right dock into two groups that persist", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    // The right dock starts as one group holding diff + terminal:0.
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    expect(await dockGroupCount(page, "right")).toBe(1);

    // Drag terminal:0 onto the group body's trailing split half: it lifts out
    // into a second side-by-side group rather than just reordering tabs.
    await dragTab(page, "terminal:0", { x: 1000, y: 400 }, async () => {
      const zone = page.getByTestId("pane-split-right-0-after");
      await expect(zone).toBeVisible();
      const box = await zone.boundingBox();
      if (box) await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 6 });
    });

    await expect.poll(() => dockGroupCount(page, "right")).toBe(2);
    // Both panes stay open, now one per group.
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();

    // The split layout round-trips through localStorage.
    await page.reload();
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect.poll(() => dockGroupCount(page, "right")).toBe(2);

    // Closing one group's pane prunes only that group; the other stays valid.
    await page.getByLabel("Close terminal").click();
    await expect(page.getByTestId("pane-tab-terminal:0")).toHaveCount(0);
    await expect.poll(() => dockGroupCount(page, "right")).toBe(1);
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
  });

  test("dragging a tab onto a pane body splits the bottom dock into two groups that persist", async ({ page }) => {
    await openSession(page);
    await page.goto(`/session/${SESSION}`);

    // Gather both panes into a single bottom-dock group.
    await page.getByLabel("Move diff to bottom dock").click();
    await page.getByLabel("Move terminal to bottom dock").click();
    await expect.poll(() => dockTabOrder(page, "bottom")).toEqual(["diff", "terminal:0"]);
    expect(await dockGroupCount(page, "bottom")).toBe(1);

    // Drag terminal:0 onto the group body's trailing split half: the wide bottom
    // strip splits side by side, so this lifts it into a second group.
    await dragTab(page, "terminal:0", { x: 640, y: 650 }, async () => {
      const zone = page.getByTestId("pane-split-bottom-0-after");
      await expect(zone).toBeVisible();
      const box = await zone.boundingBox();
      if (box) await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 6 });
    });

    await expect.poll(() => dockGroupCount(page, "bottom")).toBe(2);
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();

    // The split layout round-trips through localStorage.
    await page.reload();
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
    await expect.poll(() => dockGroupCount(page, "bottom")).toBe(2);

    // Closing one group's pane prunes only that group; the other stays valid.
    await page.getByLabel("Close terminal").click();
    await expect(page.getByTestId("pane-tab-terminal:0")).toHaveCount(0);
    await expect.poll(() => dockGroupCount(page, "bottom")).toBe(1);
    await expect(page.getByTestId("pane-tab-diff")).toBeVisible();
  });
});
