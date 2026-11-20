// The tour's last step adds install and notifications steps only in a phone's browser tab (#4595).
// The mocked server is on localhost, which counts as a secure origin.

import { devices, type Page } from "@playwright/test";
import { test, expect } from "./helpers/mockedTest";
import { DESKTOP, iPhone13 } from "./helpers/viewports";

const PHONE_375 = { width: 375, height: 667 };
const pixel7 = (({ defaultBrowserType: _browser, ...rest }) => rest)(devices["Pixel 7"]);

const IOS_LINE =
  "For notifications, add this page to your Home Screen (Share › Add to Home Screen), open it from there, sign in, and turn them on.";
const ANDROID_LINE =
  "For notifications, install this app (browser menu › Install app), open it, sign in, and turn them on.";
const LAST_TITLE = "Replay this tour any time";

/** Starts the tour from More and steps to its last step. */
async function openLastStep(page: Page) {
  await page.goto("/");
  await page.getByRole("button", { name: "More options" }).click();
  await page.getByRole("menuitem", { name: "Show tutorial" }).click();
  const step = page.getByRole("alertdialog");
  for (let i = 0; i < 12; i++) {
    await expect(step).toBeVisible();
    const next = step.getByRole("button", { name: /^Next/ });
    const done = step.getByRole("button", { name: "Done" });
    await expect(next.or(done)).toBeVisible();
    if (await done.isVisible()) break;
    const title = await step.getAttribute("aria-label");
    await next.click();
    await expect(step).not.toHaveAttribute("aria-label", title ?? "");
  }
  const last = page.getByRole("alertdialog", { name: LAST_TITLE, exact: true });
  await expect(last).toBeVisible();
  return last;
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
    await expect(last.getByText(ANDROID_LINE)).toHaveCount(0);
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

  test("the last step says how to install it from the browser menu", async ({ page }) => {
    const last = await openLastStep(page);
    await expect(last.getByText(ANDROID_LINE, { exact: true })).toBeInViewport();
    await expect(last.getByText(IOS_LINE)).toHaveCount(0);
    await expectNoSidewaysScroll(page);
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
  });
});
