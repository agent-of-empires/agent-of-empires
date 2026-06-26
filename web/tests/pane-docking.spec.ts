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
});
