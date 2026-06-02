// First-run theme selection as part of onboarding (issue #1834): a fresh
// browser shows a "Choose your theme" modal before the tour. Picking a theme
// repaints the dashboard live and persists to the default profile (survives a
// reload). The modal is suppressed in automated sessions, like the tour.
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe } from "../helpers/aoeServe";

base("theme onboarding: pick, persist, hand off to tour", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    // Present as a real (non-automated) browser so first-run onboarding runs;
    // every other live spec keeps webdriver=true and never sees the modal.
    await page.addInitScript(() => {
      Object.defineProperty(navigator, "webdriver", { get: () => false });
    });

    await page.goto(serve.baseUrl);

    // The welcome modal appears first, with the available themes as options.
    await expect(page.getByText("Choose your theme")).toBeVisible({ timeout: 10_000 });
    // Desktop viewport (>= lg): the composer and diff preview panels flank the
    // picker so the user sees the chosen theme on representative surfaces.
    await expect(page.getByText("Composer")).toBeVisible();
    await expect(page.getByText("Diff viewer")).toBeVisible();
    const options = page.getByRole("option");
    await expect(options.first()).toBeVisible();

    // Pick a theme other than the one currently applied; the dashboard repaints
    // live (dataset.theme tracks the applied theme).
    const names = await options.allInnerTexts();
    const current = await page.evaluate(() => document.documentElement.dataset.theme);
    const pick = names.find((n) => n.trim() !== current) ?? names[0];
    const picked = pick.trim();
    await page.getByRole("option", { name: picked, exact: true }).click();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.dataset.theme))
      .toBe(picked);

    // Continue dismisses the modal and hands off to the tour.
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByText("Choose your theme")).toBeHidden();
    await expect(page.getByText("Command bar")).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: "Skip" }).click();

    // The pick persisted to the profile: a reload re-applies it server-side and
    // shows neither the modal nor the tour again.
    await page.reload();
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
    await expect
      .poll(() => page.evaluate(() => document.documentElement.dataset.theme))
      .toBe(picked);
    await expect(page.getByText("Choose your theme")).toBeHidden();
  } finally {
    await serve.stop();
  }
});

base("theme onboarding: suppressed in automated sessions", async ({ page }, testInfo) => {
  const serve = await spawnAoeServe({
    authMode: "none",
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
  });

  try {
    // Default Playwright sessions report navigator.webdriver === true, so the
    // modal must never appear and the dashboard must be reachable.
    await page.goto(serve.baseUrl);
    await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByText("Choose your theme")).toBeHidden();
  } finally {
    await serve.stop();
  }
});
