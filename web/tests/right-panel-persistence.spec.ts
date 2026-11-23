import type { Page } from "@playwright/test";

import { test, expect } from "./helpers/mockedTest";
import { mockSessionShellApis, mockStaticApis } from "./helpers/apiMocks";
import { mockSessionsList } from "./helpers/sessions";
import { clickSidebarSession } from "./helpers/sidebar";

const LAYOUT_KEY = "aoe-pane-layout-v3";
const SESSIONS = ["alpha", "beta", "gamma"];

/** Serves the Daemon's panel defaults; the returned object changes what later reads see. */
async function mockPanelDefaults(page: Page) {
  const web = { auto_open_diff_pane: false, auto_open_terminal_pane: false };
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { web } }),
  );
  return web;
}

async function boot(page: Page) {
  await mockStaticApis(page);
  await mockSessionShellApis(page);
  await page.route("**/api/sessions/*/diff/files", (r) =>
    r.fulfill({
      json: {
        files: [
          { path: "a.txt", status: "modified", additions: 1, deletions: 0 },
          { path: "b.txt", status: "added", additions: 2, deletions: 0 },
        ],
        per_repo_bases: [],
        warning: null,
      },
    }),
  );
  await mockSessionsList(page, () => SESSIONS.map((id) => ({ id, title: id })));
  const defaults = await mockPanelDefaults(page);
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.goto("/");
  return defaults;
}

const dockTabs = (page: Page) => page.locator('[data-pane-dock="right"] [data-testid^="pane-tab-"]');
const savedSessions = (page: Page) =>
  page.evaluate((k) => Object.keys(JSON.parse(localStorage.getItem(k) ?? "{}").sessions ?? {}), LAYOUT_KEY);

test.describe("Side panel defaults", () => {
  test("Sessions start closed, a closed Diff shows its count, and viewing saves nothing", async ({ page }) => {
    await boot(page);
    await clickSidebarSession(page, "alpha");
    const diffToggle = page.getByTestId("pane-toggle-diff");
    await expect(diffToggle).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("pane-badge-diff")).toHaveText("2");
    await expect(dockTabs(page)).toHaveCount(0);
    await clickSidebarSession(page, "beta");
    await expect(page).toHaveURL(/\/session\/beta$/);
    expect(await savedSessions(page)).toEqual([]);
  });

  test("turning a default on reaches Sessions already viewed", async ({ page }) => {
    const defaults = await boot(page);
    const diffToggle = page.getByTestId("pane-toggle-diff");
    await clickSidebarSession(page, "alpha");
    await expect(diffToggle).toHaveAttribute("aria-pressed", "false");

    defaults.auto_open_diff_pane = true;
    await page.reload();
    await expect(diffToggle).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("pane-badge-diff")).toHaveCount(0);
    await clickSidebarSession(page, "beta");
    await expect(page).toHaveURL(/\/session\/beta$/);
    await expect(diffToggle).toHaveAttribute("aria-pressed", "true");
  });

  test("a layout the user changed survives reload while the others follow the default", async ({ page }) => {
    await boot(page);
    await clickSidebarSession(page, "alpha");
    await page.getByTestId("pane-toggle-terminal").click();
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();
    await page.reload();
    await expect(page.getByTestId("pane-tab-terminal:0")).toBeVisible();
    await clickSidebarSession(page, "beta");
    await expect(dockTabs(page)).toHaveCount(0);
  });
});
