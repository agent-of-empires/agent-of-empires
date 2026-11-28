import { test, expect } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import type { Page } from "@playwright/test";

const IMAGE = "ghcr.io/agent-of-empires/aoe-sandbox:latest";

/** A Daemon whose pull state the test moves; every status poll reads it. */
async function mockDaemon(
  page: Page,
  pull: { current: Record<string, unknown> },
  status: Record<string, unknown> = {},
) {
  await mockStaticApis(page);
  await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
    r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: { sessions: [], workspace_ordering: [] },
    }),
  );
  await page.route("**/api/sandbox/image-status**", (r) =>
    r.fulfill({
      json: {
        image: IMAGE,
        state: "current",
        dismissed: false,
        checks_enabled: true,
        sandbox_in_use: true,
        pull: pull.current,
        pull_blocked: null,
        pull_command: `docker pull ${IMAGE}`,
        runtime: "Docker",
        ...status,
      },
    }),
  );
  const cancels: number[] = [];
  await page.route("**/api/sandbox/image-pull/cancel", (r) => {
    cancels.push(Date.now());
    pull.current = { state: "cancelled" };
    return r.fulfill({ json: { image: IMAGE, pull: pull.current } });
  });
  return cancels;
}

const noSideScroll = (page: Page) =>
  page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

for (const width of [375, 1280]) {
  test(`at ${width}px a running pull fits on screen, hides to the top-bar chip and cancels in one click (#5751)`, async ({
    page,
  }) => {
    await page.setViewportSize({ width, height: 800 });
    const pull = { current: { state: "running", phase: "downloading", layers_done: 3, layers_known: 7 } };
    const cancels = await mockDaemon(page, pull);
    await page.goto("/");

    const banner = page.getByTestId("sandbox-image-banner");
    await expect(banner.getByRole("status")).toHaveText("Downloading sandbox image…");
    await expect(banner).toContainText("3 of 7 layers");
    // The bar spans 3/7 of the banner's width along its bottom edge.
    const [bar, box] = await Promise.all([
      page.getByTestId("sandbox-image-progress").boundingBox(),
      banner.boundingBox(),
    ]);
    expect(bar!.height).toBe(2);
    expect(Math.abs(bar!.y + bar!.height - (box!.y + box!.height))).toBeLessThanOrEqual(1);
    expect(Math.abs(bar!.width - (box!.width * 3) / 7)).toBeLessThanOrEqual(1);
    for (const name of ["Cancel", "Hide sandbox image notice"]) {
      await expect(banner.getByRole("button", { name })).toBeInViewport();
    }
    expect(await noSideScroll(page)).toBeLessThanOrEqual(0);

    await banner.getByRole("button", { name: "Hide sandbox image notice" }).click();
    await expect(banner).toHaveCount(0);
    const chip = page.getByRole("button", { name: "Downloading sandbox image, 3 of 7 layers. Show details" });
    await expect(chip).toBeInViewport();
    await expect(chip).toHaveText("Image 3/7");
    expect(await noSideScroll(page)).toBeLessThanOrEqual(0);
    await chip.focus();
    await page.keyboard.press("Enter");
    await expect(banner).toBeVisible();
    await expect(chip).toHaveCount(0);

    await banner.getByRole("button", { name: "Cancel" }).click();
    await expect(banner.getByRole("status")).toHaveText("Update cancelled. Sessions keep the current image.");
    await expect(banner.getByRole("button", { name: "Update now" })).toBeInViewport();
    expect(cancels).toHaveLength(1);
  });
}

test("the pull spinner stands still when the user prefers reduced motion (#5751)", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await mockDaemon(page, { current: { state: "running" } });
  await page.goto("/");
  const spinner = page.getByTestId("sandbox-image-banner").locator("svg.animate-spin");
  await expect(spinner).toBeVisible();
  expect(await spinner.evaluate((el) => getComputedStyle(el).animationName)).toBe("none");
  await page.emulateMedia({ reducedMotion: "no-preference" });
  expect(await spinner.evaluate((el) => getComputedStyle(el).animationName)).not.toBe("none");
});

test("at 375px a refused Cancel says so, and its Details fit on screen (#5751)", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  await mockDaemon(page, { current: { state: "running", phase: "downloading", layers_done: 3, layers_known: 7 } });
  await page.route("**/api/sandbox/image-pull/cancel", (r) =>
    r.fulfill({
      status: 403,
      json: { error: "pull_disabled", message: "Pulling sandbox images from the web is off." },
    }),
  );
  await page.goto("/");
  const banner = page.getByTestId("sandbox-image-banner");
  await banner.getByRole("button", { name: "Cancel" }).click();
  await expect(banner.getByRole("status")).toHaveText("Downloading sandbox image… Couldn't cancel.");
  await banner.getByRole("button", { name: "Details" }).click();
  await expect(page.getByText("Pulling sandbox images from the web is off.")).toBeInViewport();
  for (const name of ["Cancel", "Details", "Hide sandbox image notice"]) {
    await expect(banner.getByRole("button", { name })).toBeInViewport();
  }
  expect(await noSideScroll(page)).toBeLessThanOrEqual(0);
});

test("at 375px the update offer's Why? fits on screen", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 800 });
  await mockDaemon(page, { current: { state: "idle" } }, { state: "available", remote_digest: "sha256:b" });
  await page.goto("/");
  const banner = page.getByTestId("sandbox-image-banner");
  await expect(banner.getByRole("status")).toHaveText("Sandbox image update available");
  await banner.getByRole("button", { name: "Why?" }).click();
  await expect(page.getByText("Sandboxed Sessions run in the current image.", { exact: false })).toBeInViewport();
  for (const name of ["Update now", "Why?", "Dismiss sandbox image notice"]) {
    await expect(banner.getByRole("button", { name })).toBeInViewport();
  }
  expect(await noSideScroll(page)).toBeLessThanOrEqual(0);
});
