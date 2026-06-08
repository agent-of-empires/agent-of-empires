// User stories (issue #1513): the first-run tutorial auto-launches on a fresh
// browser, is skippable, persists "seen" so it does not nag on reload, and is
// re-triggerable from the TopBar overflow menu.
//
// Since #1832 the "seen" flag lives server-side (config.toml
// `app_state.has_seen_web_tour`) rather than per-browser localStorage, so it
// follows the user across browsers/devices. This spec asserts the backend flag
// via GET /api/settings.
//
// A fresh `aoe serve` $HOME has no sessions, so the app lands on the empty
// dashboard and the dashboard-scope tour auto-launches (Playwright is a
// fine-pointer client, so the coarse-pointer suppression does not apply).
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

// First dashboard step's title (TOUR_STEPS[0] = topbar -> "Command bar").
const FIRST_STEP = "Command bar";

base(
  "first-run tutorial: auto-launch, skip, persist, re-trigger",
  async ({ page }, testInfo) => {
    const serve = await spawnAoeServe({
      authMode: "none",
      workerIndex: testInfo.workerIndex,
      parallelIndex: testInfo.parallelIndex,
    });

    try {
      // Auto-launch is suppressed in automated sessions (navigator.webdriver) so
      // the spotlight overlay never intercepts clicks in the rest of the suite.
      // This spec is the one place that exercises auto-launch, so present as a
      // real (non-automated) browser. Persists across reloads/navigations.
      await page.addInitScript(() => {
        Object.defineProperty(navigator, "webdriver", { get: () => false });
      });

      await page.goto(serve.baseUrl);

      // Phase 1 of onboarding (#1834): the theme welcome modal shows first on a
      // fresh browser. Dismiss it; the tour then takes over.
      await expect(page.getByText("Choose your theme")).toBeVisible({
        timeout: 10_000,
      });
      await page.getByRole("button", { name: "Continue" }).click();

      // Story 1: tour auto-launches once the welcome closes, with a Skip button.
      await expect(page.getByText(FIRST_STEP)).toBeVisible({ timeout: 10_000 });
      const skip = page.getByRole("button", { name: "Skip" });
      await expect(skip).toBeVisible();

      // Skipping closes the tour and records the seen flag on the server.
      const postSeen = page.waitForResponse(
        (r) =>
          r.url().includes("/api/app-state/web-tour-seen") &&
          r.request().method() === "POST",
        { timeout: 10_000 },
      );
      await skip.click();
      const resp = await postSeen;
      expect(resp.status()).toBe(200);
      await expect(page.getByText(FIRST_STEP)).toBeHidden();
      await expect
        .poll(() =>
          page.evaluate(async () => {
            const res = await fetch("/api/settings", { cache: "no-store" });
            if (!res.ok) return false;
            const cfg = await res.json();
            return cfg?.app_state?.has_seen_web_tour === true;
          }),
        )
        .toBe(true);

      // Story 1 (persistence): a reload must not auto-launch the tour or
      // re-show the theme welcome modal.
      await page.reload();
      await expect(
        page.getByRole("button", { name: "Go to dashboard" }),
      ).toBeVisible();
      await expect(page.getByText(FIRST_STEP)).toBeHidden();
      await expect(page.getByText("Choose your theme")).toBeHidden();

      // Story 2: re-trigger from the fixed entry point (TopBar overflow menu).
      await page.getByRole("button", { name: "More options" }).click();
      await page.getByRole("menuitem", { name: "Show tutorial" }).click();
      await expect(page.getByText(FIRST_STEP)).toBeVisible({ timeout: 10_000 });

      // The flag stays set after a manual re-trigger, so the next reload is quiet.
      expect(
        await page.evaluate(async () => {
          const res = await fetch("/api/settings");
          if (!res.ok) return false;
          const cfg = await res.json();
          return cfg?.app_state?.has_seen_web_tour === true;
        }),
      ).toBe(true);
    } finally {
      await serve.stop();
    }
  },
);
