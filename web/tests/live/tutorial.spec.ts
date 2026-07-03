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

base("first-run tutorial: auto-launch, skip, persist, re-trigger", async ({ page }, testInfo) => {
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

    // Tips also auto-pop for returning users (tour seen) in a non-automated
    // session. This spec reloads into exactly that state, so turn tips off here
    // to keep the tour the only modal under test; tips have their own spec.
    const disableTipsRes = await page.request.post(`${serve.baseUrl}/api/tips/show`, {
      data: { enabled: false },
    });
    expect(disableTipsRes.ok(), "failed to disable tips before tutorial isolation").toBeTruthy();

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
      (r) => r.url().includes("/api/app-state/web-tour-seen") && r.request().method() === "POST",
      { timeout: 10_000 },
    );
    await skip.click();
    const resp = await postSeen;
    expect(resp.status()).toBe(200);
    await expect(page.getByText(FIRST_STEP)).toBeHidden();
    // The POST returns 200 before the server has flushed the flag to
    // config.toml, so GET /api/settings can briefly still report false.
    // Poll with the same 10s budget the rest of this spec uses; the default
    // 5s poll window is too tight under CI load and flakes here.
    await expect
      .poll(
        () =>
          page.evaluate(async () => {
            const res = await fetch("/api/settings", { cache: "no-store" });
            if (!res.ok) return false;
            const cfg = await res.json();
            return cfg?.app_state?.has_seen_web_tour === true;
          }),
        { timeout: 10_000 },
      )
      .toBe(true);

    // Story 1 (persistence): a reload must not auto-launch the tour or
    // re-show the theme welcome modal.
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect(page.getByText(FIRST_STEP)).toBeHidden();
    await expect(page.getByText("Choose your theme")).toBeHidden();

    // Story 2: re-trigger from the fixed entry point (TopBar overflow menu).
    await page.getByRole("button", { name: "More options" }).click();
    await page.getByRole("menuitem", { name: "Show tutorial" }).click();
    await expect(page.getByText(FIRST_STEP)).toBeVisible({ timeout: 10_000 });

    // Story (issue #2633): the tour opens Settings mid-walk. The controlled
    // runner navigates into the Worktree then Plugins tab, mounting each anchor
    // before showing its step, and closes Settings when it moves on.
    // Joyride labels the advance button "Next (N of M)", so match on the prefix.
    // Walk step by step, waiting for each tooltip before advancing: the crossing
    // into Settings is async (navigate, suspend, poll for the anchor, remount),
    // so a tight click loop would race past the worktree step before it paints.
    const next = page.getByRole("button", { name: /^Next/ });
    const step = async (heading: string) => {
      await next.click();
      await expect(page.getByText(heading)).toBeVisible({ timeout: 10_000 });
    };
    await step("Workspaces and sessions"); // sidebar
    await step("Start a session"); // new-session
    await step("Settings and profiles"); // settings entry
    await step("Worktrees keep sessions isolated"); // opens Settings > Worktree
    await expect.poll(() => new URL(page.url()).pathname).toBe("/settings/worktree");

    await step("Extend AoE with plugins"); // switches to Settings > Plugins
    await expect.poll(() => new URL(page.url()).pathname).toBe("/settings/plugins");

    // Moving past the plugins step closes Settings and lands back on the dashboard.
    await step("Replay this tour any time");
    await expect.poll(() => new URL(page.url()).pathname).toBe("/");
    await page.getByRole("button", { name: /^Done/ }).click();
    await expect(page.getByText("Replay this tour any time")).toBeHidden();

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
});
