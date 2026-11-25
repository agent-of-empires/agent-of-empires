// A phone's browser tab gets notification steps in the install banner and the tour's last step (#4595).
// The mocked server is on localhost, which counts as a secure origin.

import { devices, type Page } from "@playwright/test";
import { mockSettingsApis, mockStaticApis } from "./helpers/apiMocks";
import { test, expect } from "./helpers/mockedTest";
import { DESKTOP, iPhone13 } from "./helpers/viewports";

const PHONE_375 = { width: 375, height: 667 };
const pixel7 = (({ defaultBrowserType: _browser, ...rest }) => rest)(devices["Pixel 7"]);

const IOS_LINE = "For notifications, tap Share › Add to Home Screen, open AoE, sign in, and turn them on in Settings.";
const PUSH_LINE = "For notifications, turn them on in Settings › Notifications.";
const BANNER = "Get notifications on this device";
const LAST_TITLE = "Replay this tour any time";

/** Starts the tour from More and steps to its last step. */
async function openLastStep(page: Page) {
  await page.goto("/");
  await page.getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitem", { name: "Show tutorial" }).click();
  const step = page.getByRole("alertdialog");
  const next = step.getByRole("button", { name: /^Next/ });
  const done = step.getByRole("button", { name: "Done" });
  // Far above the tour's length; it only stops a tour that never reaches Done.
  const maxSteps = 20;
  for (let steps = 0; ; steps++) {
    await expect(next.or(done)).toBeVisible();
    if (await done.isVisible()) break;
    expect(steps, "the tour never reached Done").toBeLessThan(maxSteps);
    const title = await step.getAttribute("aria-label");
    await next.click();
    await expect(step).not.toHaveAttribute("aria-label", title ?? "");
  }
  const last = page.getByRole("alertdialog", { name: LAST_TITLE, exact: true });
  await expect(last).toBeVisible();
  return last;
}

/** Opens the dashboard on a reachable Daemon with push on, and returns the install banner. */
async function openBanner(page: Page) {
  await mockStaticApis(page);
  await mockSettingsApis(page);
  await page.route("**/api/push/status*", (r) => r.fulfill({ json: { enabled: true } }));
  await page.goto("/");
  const banner = page.getByRole("region", { name: BANNER });
  await expect(banner).toBeVisible();
  return banner;
}

async function expectNoSidewaysScroll(page: Page) {
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(0);
}

test.describe("iPhone Safari tab", () => {
  test.use({ ...iPhone13, viewport: PHONE_375 });

  test("the last step says how to add it to the Home Screen", async ({ page }) => {
    const last = await openLastStep(page);
    await expect(last.getByText(IOS_LINE, { exact: true })).toBeInViewport();
    await expect(last.getByRole("button", { name: "Done" })).toBeInViewport();
    await expect(last.getByText(PUSH_LINE)).toHaveCount(0);
    await expectNoSidewaysScroll(page);
  });

  test("the banner gives the Home Screen steps and fits the screen", async ({ page }) => {
    const banner = await openBanner(page);
    await expect(banner.getByText("Share › Add to Home Screen", { exact: true })).toBeInViewport();
    await expect(banner.getByRole("button", { name: "Hide notification steps" })).toBeInViewport();
    await expectNoSidewaysScroll(page);
  });

  test("an installed app gets no install line", async ({ page }) => {
    await page.addInitScript(() => Object.defineProperty(navigator, "standalone", { get: () => true }));
    const last = await openLastStep(page);
    await expect(last.getByText(/For notifications/)).toHaveCount(0);
  });
});

test.describe("Android Chrome tab", () => {
  test.use({ ...pixel7, viewport: PHONE_375 });

  test("the last step says to turn them on in Settings", async ({ page }) => {
    const last = await openLastStep(page);
    await expect(last.getByText(PUSH_LINE, { exact: true })).toBeInViewport();
    await expect(last.getByText(IOS_LINE)).toHaveCount(0);
    await expectNoSidewaysScroll(page);
  });

  test("the banner links to Settings › Notifications and fits the screen", async ({ page }) => {
    // Headless Chromium reports permission denied, which hides the banner; a fresh tab hasn't been asked.
    await page.addInitScript(() => Object.defineProperty(Notification, "permission", { get: () => "default" }));
    const banner = await openBanner(page);
    await expect(banner.getByText(/Add to Home Screen/)).toHaveCount(0);
    await expectNoSidewaysScroll(page);
    await banner.getByRole("link", { name: "Settings › Notifications" }).click();
    await expect(page).toHaveURL(/\/settings\/notifications$/);
    await expect(page.getByRole("button", { name: "Enable notifications" })).toBeVisible();
  });
});

test.describe("desktop", () => {
  test.use(DESKTOP);

  test("the last step shows only the replay line", async ({ page }) => {
    const last = await openLastStep(page);
    await expect(
      last.getByText("Reopen this walkthrough whenever you like from here: More, then Show tutorial."),
    ).toBeVisible();
    await expect(last.getByText(/For notifications/)).toHaveCount(0);
    await expect(page.getByRole("region", { name: BANNER })).toHaveCount(0);
  });
});
