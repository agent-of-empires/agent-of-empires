// First-run theme selection as part of onboarding (issue #1834): a fresh
// browser shows a "Choose your theme" modal before the tour. Picking a theme
// repaints the dashboard live and PATCHes the global /api/theme endpoint;
// dismissing hands off to the tour. The modal is suppressed in automated
// sessions, like the tour. Ported from live to the mocked suite: stateful
// stubs for /api/theme(+current) and the app_state tour flag let the reload
// assertions exercise the same fetch-and-reapply path the real backend feeds.

import { test, expect } from "./helpers/mockedTest";
import type { Page } from "@playwright/test";

interface ResolvedThemePayload {
  name: string;
  source: "builtin";
  appearance: "dark";
  web: { cssVars: Record<string, string> };
  terminal: { cssVars: Record<string, string> };
  syntax: { shikiTheme: string };
}

function resolved(name: string, surface: string): ResolvedThemePayload {
  return {
    name,
    source: "builtin",
    appearance: "dark",
    web: { cssVars: { "--color-surface-900": surface } },
    terminal: { cssVars: { "--term-bg": surface } },
    syntax: { shikiTheme: "github-dark" },
  };
}

const THEMES: Record<string, ResolvedThemePayload> = {
  empire: resolved("empire", "#0f172a"),
  dracula: resolved("dracula", "#282a36"),
};

interface OnboardingMockHandle {
  /** Current theme name; PATCH /api/theme updates it so a reload re-applies
   *  the pick via /api/theme/current. */
  current: string;
  themePatches: Array<{ name?: string; color_mode?: string }>;
  /** Backend tour-seen flag (app_state.has_seen_web_tour). */
  tourSeen: boolean;
}

async function installOnboardingMocks(page: Page): Promise<OnboardingMockHandle> {
  const handle: OnboardingMockHandle = {
    current: "empire",
    themePatches: [],
    tourSeen: false,
  };

  await page.route(
    (url) => url.pathname === "/api/sessions",
    (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }),
  );
  await page.route(
    (url) => url.pathname === "/api/about",
    (r) =>
      r.fulfill({
        json: { read_only: false, auth_mode: "none", behind_tunnel: false, profile: "main" },
      }),
  );
  // tourSeenKnown gates the welcome decision; it flips only once this
  // settings fetch resolves, so it must be stubbed (a failed fetch keeps the
  // modal suppressed forever).
  await page.route(
    (url) => url.pathname === "/api/settings",
    (r) => r.fulfill({ json: { app_state: { has_seen_web_tour: handle.tourSeen } } }),
  );
  await page.route(
    (url) => url.pathname === "/api/app-state/web-tour-seen",
    (route) => {
      handle.tourSeen = true;
      return route.fulfill({ json: { ok: true } });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/themes",
    (r) => r.fulfill({ json: Object.keys(THEMES) }),
  );
  await page.route(
    (url) => /^\/api\/themes\/[^/]+$/.test(url.pathname),
    (route) => {
      const name = decodeURIComponent(new URL(route.request().url()).pathname.split("/").pop() ?? "");
      const payload = THEMES[name];
      if (!payload) return route.fulfill({ status: 404, body: "not found" });
      return route.fulfill({ json: payload });
    },
  );
  await page.route(
    (url) => url.pathname === "/api/theme/current",
    (r) => r.fulfill({ json: THEMES[handle.current] }),
  );
  await page.route(
    (url) => url.pathname === "/api/theme",
    (route) => {
      const body = route.request().postDataJSON() as { name?: string };
      handle.themePatches.push(body);
      if (body?.name) handle.current = body.name;
      return route.fulfill({ json: { ok: true } });
    },
  );

  return handle;
}

function datasetTheme(page: Page) {
  return page.evaluate(() => document.documentElement.dataset.theme);
}

test("theme onboarding: pick, persist, hand off to tour", async ({ page }) => {
  const handle = await installOnboardingMocks(page);

  // Present as a real (non-automated) browser so first-run onboarding runs;
  // every other mocked spec keeps webdriver=true and never sees the modal.
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "webdriver", { get: () => false });
  });

  await page.goto("/");

  // The welcome modal appears first, with the available themes as options.
  await expect(page.getByText("Choose your theme")).toBeVisible();
  // Desktop viewport (>= lg): the composer and diff preview panels flank the
  // picker so the user sees the chosen theme on representative surfaces.
  await expect(page.getByText("Composer")).toBeVisible();
  await expect(page.getByText("Diff viewer")).toBeVisible();

  // Pick a theme other than the current one; the dashboard repaints live
  // (dataset.theme tracks the applied theme) and the pick PATCHes the global
  // /api/theme endpoint, never a profile.
  await expect.poll(() => datasetTheme(page)).toBe("empire");
  await page.getByRole("option", { name: "dracula", exact: true }).click();
  await expect.poll(() => handle.themePatches).toEqual([{ name: "dracula" }]);
  await expect.poll(() => datasetTheme(page)).toBe("dracula");

  // Continue dismisses the modal and hands off to the tour.
  await page.getByRole("button", { name: "Continue" }).click();
  await expect(page.getByText("Choose your theme")).toBeHidden();
  await expect(page.getByText("Command bar")).toBeVisible();
  await page.getByRole("button", { name: "Skip" }).click();
  await expect.poll(() => handle.tourSeen).toBe(true);

  // The pick persisted: a reload re-applies it via /api/theme/current and
  // shows neither the modal nor the tour again (welcome flag is per-browser,
  // tour flag is backend app_state).
  await page.reload();
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
  await expect.poll(() => datasetTheme(page)).toBe("dracula");
  await expect(page.getByText("Choose your theme")).toBeHidden();
  await expect(page.getByText("Command bar")).toBeHidden();
});

test("theme onboarding: suppressed in automated sessions", async ({ page }) => {
  await installOnboardingMocks(page);

  // Default Playwright sessions report navigator.webdriver === true, so the
  // modal must never appear and the dashboard must be reachable.
  await page.goto("/");
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
  await expect(page.getByText("Choose your theme")).toBeHidden();
});
