// #4595: an iPhone tab finds how to get notifications from the install banner, whether or not the tour ran.
// The harness serves on 127.0.0.1, which counts as a secure origin.

import { devices, type Page } from "@playwright/test";
import { test, expect } from "../helpers/liveTest";

test.use({ ...devices["iPhone 13"] });

const BANNER = "Get notifications on this device";
const HIDE = "Hide notification steps";
const IOS_STEPS = [
  "Tap Share › Add to Home Screen.",
  "Open AoE from your Home Screen and sign in.",
  "Come back here and tap Enable notifications.",
];

/** Auto-launching onboarding is suppressed for automated browsers; present as a real one. */
const presentAsRealBrowser = (page: Page) =>
  page.addInitScript(() => Object.defineProperty(navigator, "webdriver", { get: () => false }));

/** Leaves setup for later and turns tips off, so `/` opens the dashboard with no other modal. */
async function openDashboard(page: Page, baseUrl: string) {
  expect((await page.request.patch(`${baseUrl}/api/setup`, { data: { deferred: true } })).ok()).toBeTruthy();
  expect((await page.request.post(`${baseUrl}/api/tips/show`, { data: { enabled: false } })).ok()).toBeTruthy();
  await presentAsRealBrowser(page);
  // iOS Safari exposes push only to Home Screen apps; Chromium's iPhone emulation still has it.
  await page.addInitScript(() => delete (window as { PushManager?: unknown }).PushManager);
  await page.goto(baseUrl);
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
}

const tourSeen = (page: Page) =>
  page.evaluate(async () => {
    const res = await fetch("/api/settings", { cache: "no-store" });
    return res.ok ? (await res.json())?.app_state?.has_seen_web_tour === true : null;
  });

test("a phone that never sees the tour, because a desktop finished it, finds the iOS steps", async ({
  page,
  serve,
}) => {
  // The seen flag is server-wide (#1832), so a desktop finishing the tour hides it from every phone.
  expect((await page.request.post(`${serve.baseUrl}/api/app-state/web-tour-seen`)).ok()).toBeTruthy();
  await openDashboard(page, serve.baseUrl);
  const banner = page.getByRole("region", { name: BANNER });
  await expect(banner).toBeVisible();
  await expect(banner.getByText("Share › Add to Home Screen", { exact: true })).toBeVisible();
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
});

test("a phone on a fresh server finds the banner without the tour opening", async ({ page, serve }) => {
  await openDashboard(page, serve.baseUrl);
  await expect(page.getByRole("region", { name: BANNER })).toBeVisible();
  // A phone never auto-launches the tour, and the banner leaves it unseen.
  await expect(page.getByRole("alertdialog")).toHaveCount(0);
  expect(await tourSeen(page)).toBe(false);
});

test("× hides the banner across a reload, and Settings › Notifications keeps the steps", async ({ page, serve }) => {
  await openDashboard(page, serve.baseUrl);
  const banner = page.getByRole("region", { name: BANNER });
  await banner.getByRole("button", { name: HIDE }).click();
  await expect(banner).toHaveCount(0);
  await page.reload();
  await expect(page.getByRole("button", { name: "Go to dashboard" })).toBeVisible();
  await expect(page.getByRole("region", { name: BANNER })).toHaveCount(0);

  await page.goto(`${serve.baseUrl}/settings/notifications`);
  await page.getByText("How to install on iPhone or iPad").click();
  await expect(page.getByRole("listitem").filter({ hasText: /Home Screen|Enable notifications/ })).toHaveText(
    IOS_STEPS,
  );
});
