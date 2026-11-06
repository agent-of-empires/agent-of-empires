// First-run setup wizard (#5395): a fresh writable dashboard redirects `/` to `/setup` without the dashboard
// flashing first, a finished one shows the leftover-steps banner, and read-only viewers get neither. Automated
// browsers skip the redirect, so each test presents as a real one.

import type { Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";

const KEYS = ["theme_telemetry", "agents", "project", "worktrees", "sandbox", "notifications"];

interface Handle {
  finished: boolean;
  dismissed: boolean;
  patches: unknown[];
  themePatches: unknown[];
}

async function installSetupMocks(page: Page, opts: { finished?: boolean; readOnly?: boolean } = {}): Promise<Handle> {
  const handle: Handle = { finished: opts.finished ?? false, dismissed: false, patches: [], themePatches: [] };
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "webdriver", { get: () => false });
    // Records whether the dashboard ever rendered, to prove the redirect never flashes it.
    new MutationObserver(() => {
      if (document.body?.textContent?.includes("Pick a project, then launch a new session")) {
        (window as typeof window & { __dashboardSeen?: boolean }).__dashboardSeen = true;
      }
    }).observe(document, { childList: true, subtree: true });
  });
  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) =>
      r.fulfill({
        headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
        json: { sessions: [], workspace_ordering: [] },
      }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) => r.fulfill({ json: { read_only: !!opts.readOnly, auth_mode: "none", profile: "main" } }),
  );
  await page.route(
    (url) => url.pathname === "/api/setup",
    async (route) => {
      if (route.request().method() === "PATCH") {
        const body = route.request().postDataJSON() as { finished?: boolean; banner_dismissed?: boolean };
        handle.patches.push(body);
        if (body.finished) handle.finished = true;
        if (body.banner_dismissed) handle.dismissed = true;
        return route.fulfill({ status: 204 });
      }
      return route.fulfill({
        json: {
          finished: handle.finished,
          deferred: false,
          banner_dismissed: handle.dismissed,
          tmux: true,
          git: true,
          access: { serve_mode: "local" },
          steps: KEYS.map((key) => ({
            key,
            required: key === "agents",
            configured: key === "agents",
            confirmed: false,
            skipped: key === "sandbox",
          })),
        },
      });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { app_state: { has_seen_web_tour: true } } }),
  );
  await page.route(
    (url) => url.pathname === "/api/settings/schema",
    (r) => r.fulfill({ json: [] }),
  );
  await page.route(
    (url) => url.pathname === "/api/telemetry/status",
    (r) => r.fulfill({ json: { enabled: false, responded: false, do_not_track: false } }),
  );
  await page.route(
    (url) => url.pathname === "/api/themes",
    (r) => r.fulfill({ json: ["empire", "dracula"] }),
  );
  await page.route(
    (url) => url.pathname === "/api/theme",
    (route) => {
      handle.themePatches.push(route.request().postDataJSON());
      return route.fulfill({ json: { ok: true } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/agents",
    (r) =>
      r.fulfill({
        json: [{ name: "claude", display_name: "Claude Code", kind: "builtin", installed: true, acp_capable: false }],
      }),
  );
  return handle;
}

const dashboardSeen = (page: Page) =>
  page.evaluate(() => (window as typeof window & { __dashboardSeen?: boolean }).__dashboardSeen === true);

test("a fresh install lands on /setup without the dashboard flashing first", async ({ page }) => {
  const handle = await installSetupMocks(page);
  await page.goto("/");
  await expect(page).toHaveURL(/\/setup$/);
  await expect(page.getByRole("heading", { name: "Theme and telemetry" })).toBeVisible();
  expect(await dashboardSeen(page)).toBe(false);

  // Picking a theme writes the global theme endpoint.
  await page.getByRole("radio", { name: "dracula" }).check();
  await expect.poll(() => handle.themePatches).toEqual([{ name: "dracula" }]);

  // The rail jumps to any step; Skip is hidden on the required one.
  await page
    .getByRole("navigation", { name: "Setup steps" })
    .getByRole("button", { name: /Agents/ })
    .click();
  await expect(page.getByRole("heading", { name: "Agents" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Skip for now" })).toHaveCount(0);
});

test("a finished setup shows the leftover steps on the dashboard, and the banner can be dismissed", async ({
  page,
}) => {
  const handle = await installSetupMocks(page, { finished: true });
  await page.goto("/");
  const banner = page.getByRole("region", { name: "Finish setting up AoE" });
  await expect(banner).toBeVisible();
  await expect(page).toHaveURL(/\/$/);
  await expect(banner.getByText("1 of 6 done. Nothing here blocks you.")).toBeVisible();
  await expect(banner.getByRole("link", { name: "Settings › Sandbox" })).toBeVisible();
  await banner.getByRole("button", { name: "Dismiss setup checklist" }).click();
  await expect.poll(() => handle.patches).toEqual([{ banner_dismissed: true }]);
  await expect(banner).toHaveCount(0);
});

test("a read-only viewer is never redirected and sees no setup banner", async ({ page }) => {
  await installSetupMocks(page, { readOnly: true });
  await page.goto("/");
  await expect(page.getByText("This dashboard is in read-only mode.")).toBeVisible();
  await expect(page).toHaveURL(/\/$/);
  await expect(page.getByRole("region", { name: "Finish setting up AoE" })).toHaveCount(0);
});

test("on a phone the rail folds into a step sheet", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await installSetupMocks(page);
  await page.goto("/setup");
  await expect(page.getByText("Step 1 of 6", { exact: true })).toBeVisible();
  const stepsButton = page.getByRole("button", { name: "Steps" });
  const sheet = page.getByRole("dialog", { name: "Setup steps" });
  // Escape closes the sheet and focus returns to its opener.
  await stepsButton.click();
  await expect(sheet).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(sheet).toHaveCount(0);
  await expect(stepsButton).toBeFocused();
  await stepsButton.click();
  await sheet.getByRole("button", { name: /Worktrees/ }).click();
  await expect(sheet).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "Worktrees" })).toBeVisible();
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  expect(overflow).toBe(false);
});
