// Mocked-Playwright coverage for plugin panes on mobile (#2514).
//
// Below the md breakpoint the layout collapses to a single full-viewport pane
// chosen by the bottom-sheet picker, which used to hardcode agent / diff /
// paired. A plugin "pane" slot (e.g. the github plugin's PR tool-window)
// rendered as a dock tab on desktop but had no path on mobile at all. The
// picker now lists plugin panes too, and selecting one promotes PluginPaneBody
// into the main pane with the usual back-to-agent control.

import { test, expect } from "./helpers/mockedTest";
import { devices, type Page } from "@playwright/test";
import { clickSidebarSession, openMobileSidebar } from "./helpers/sidebar";
import { mockTerminalApis } from "./helpers/terminal-mocks";

test.use({ ...devices["iPhone 13"] });

// One plugin pane scoped to the seeded "pinch-test" session, shaped like the
// github plugin's `github_pane`. Its id resolves to "plugin:acme.kit:gh".
const PANE_ENTRY = {
  plugin_id: "acme.kit",
  slot: "pane",
  id: "gh",
  session_id: "pinch-test",
  payload: {
    title: "GitHub",
    default_location: "right",
    icon: "git-pull-request",
    blocks: [
      { kind: "heading", text: "GitHub" },
      { kind: "note", text: "PR #1 open" },
    ],
  },
};

const PANE_ID = "plugin:acme.kit:gh";

async function setupSession(page: Page) {
  await mockTerminalApis(page);
  // Registered after mockTerminalApis so this handler wins for ui-state.
  await page.route("**/api/plugins/ui-state", (r) => r.fulfill({ json: { entries: [PANE_ENTRY], notifications: [] } }));
  await page.goto("/");
  await openMobileSidebar(page);
  await clickSidebarSession(page, "pinch-test");
  await page.locator("[data-live-terminal]").first().waitFor({ state: "visible", timeout: 10_000 });
}

async function openPicker(page: Page) {
  await page.getByRole("button", { name: "Toggle panels" }).click();
  await page.getByTestId("mobile-right-panel-picker").waitFor({ state: "visible", timeout: 5_000 });
}

test.describe("Mobile plugin pane picker (#2514)", () => {
  test("picker lists the plugin pane and promotes it into the main pane", async ({ page }) => {
    await setupSession(page);
    await openPicker(page);

    const option = page.getByTestId(`mobile-right-panel-pick-${PANE_ID}`);
    await expect(option).toBeVisible();
    await expect(option).toContainText("GitHub");

    await option.click();
    // Picker closes; the plugin pane body mounts full-viewport.
    await expect(page.getByTestId("mobile-right-panel-picker")).toHaveCount(0);
    await expect(page.getByTestId("plugin-pane-body")).toBeVisible();
    await expect(page.getByTestId("plugin-pane-body")).toContainText("PR #1 open");

    // The plugin view carries the persistent back-to-agent affordance.
    const back = page.getByTestId("mobile-back-to-agent");
    await expect(back).toBeVisible();
    await back.click();
    await expect(page.getByTestId("mobile-back-to-agent")).toHaveCount(0);
    await expect(page.locator("[data-live-terminal]").first()).toBeVisible();
  });
});
