import { test, expect, waitForResponseBody, observeFor } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import { Page } from "@playwright/test";

async function mockBase(page: Page) {
  await mockStaticApis(page);
  await page.route(/\/api\/sessions(\?.*)?$/, (r) =>
    r.fulfill({
      headers: { "aoe-runtime-epoch": "mock-runtime", "aoe-runtime-revision": "1" },
      json: { sessions: [], workspace_ordering: [] },
    }),
  );
}

test.describe("Update banner (#984, #1140)", () => {
  test("renders a newer release past an older dismissal; dismiss persists per-version across reload (server-side)", async ({
    page,
  }) => {
    // Server-backed dismissal: clicking Dismiss POSTs the version, and
    // subsequent update-status polls (this device or any other) report it
    // dismissed. Modelled with a mutable server-side flag.
    // An older dismissed version does not suppress a newer release.
    let dismissed: string | null = "0.5.5";
    await mockBase(page);
    let dismissPosted = false;
    await page.route("**/api/app-state/dismiss-update", async (r) => {
      const body = JSON.parse(r.request().postData() || "{}") as { version?: string };
      dismissed = body.version ?? null;
      dismissPosted = true;
      await r.fulfill({ json: { dismissed_version: dismissed } });
    });
    await page.route("**/api/system/update-status", (r) =>
      r.fulfill({
        json: {
          update_check_mode: "notify",
          current_version: "0.5.0",
          latest_version: "0.6.0",
          update_available: true,
          release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
          error: null,
          dismissed_version: dismissed,
        },
      }),
    );

    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("v0.5.0");
    await expect(banner).toContainText("v0.6.0");
    await expect(banner.getByRole("link", { name: "Release notes" })).toHaveAttribute(
      "href",
      "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
    );
    await page.getByRole("button", { name: /Dismiss update notice/i }).click();
    await expect(banner).toHaveCount(0);
    await expect.poll(() => dismissPosted).toBe(true);
    expect(dismissed).toBe("0.6.0");

    // Reload: the server now reports the version dismissed, so the banner
    // stays hidden without any per-browser state.
    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    await waitForResponseBody(page, "/api/system/update-status");
    await observeFor(page, 300, async () => {
      expect(await page.getByRole("status", { name: /Update available/i }).count()).toBe(0);
    });
  });

  test("at 375px the longest command wraps inside its box and nothing scrolls sideways (#4720)", async ({ page }) => {
    const cargo = "cargo install --git https://github.com/agent-of-empires/agent-of-empires aoe";
    await mockBase(page);
    await page.route("**/api/system/update-status", (r) =>
      r.fulfill({
        json: {
          update_check_mode: "notify",
          current_version: "1.19.0",
          latest_version: "1.20.0",
          update_available: true,
          release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v1.20.0",
          error: null,
          dismissed_version: null,
          install_method: "cargo",
          update_command: cargo,
        },
      }),
    );
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto("/");
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    await expect(banner.getByText(cargo)).toBeVisible();
    // The restart qualifier is visible text, not a tooltip.
    await expect(banner.getByText("or restart it through your service manager")).toBeVisible();
    const box = (await banner.boundingBox())!;
    for (const name of [`Copy ${cargo}`, "Copy aoe serve --restart", "Dismiss update notice"]) {
      const control = (await banner.getByRole("button", { name }).boundingBox())!;
      expect(control.height, name).toBeGreaterThanOrEqual(32);
      expect(control.x + control.width, name).toBeLessThanOrEqual(box.x + box.width + 0.5);
    }
    expect(
      await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth),
    ).toBeLessThanOrEqual(0);
    expect(await banner.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(0);
    // #5514: on a phone the lines stack left-aligned and × stays at the top right.
    const dismiss = (await banner.getByRole("button", { name: "Dismiss update notice" }).boundingBox())!;
    expect(dismiss.y - box.y).toBeLessThanOrEqual(12);
    expect(box.x + box.width - (dismiss.x + dismiss.width)).toBeLessThanOrEqual(12);
    const lines = banner.locator("[data-banner-line]");
    const [first, second] = [(await lines.nth(0).boundingBox())!, (await lines.nth(1).boundingBox())!];
    expect(Math.abs(first.x - second.x)).toBeLessThanOrEqual(1);
    expect(first.x - box.x).toBeLessThanOrEqual(40);
  });

  test("on a wide screen the banner's text is centered, × is pinned right, and line two starts under line one (#5514)", async ({
    page,
  }) => {
    await mockBase(page);
    await page.route("**/api/system/update-status", (r) =>
      r.fulfill({
        json: {
          update_check_mode: "notify",
          current_version: "1.21.0",
          latest_version: "1.22.1",
          update_available: true,
          release_url: null,
          error: null,
          dismissed_version: null,
          install_method: "homebrew",
          update_command: "aoe update",
        },
      }),
    );
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    const box = (await banner.boundingBox())!;
    const lines = banner.locator("[data-banner-line]");
    await expect(lines).toHaveCount(2);
    const [first, second] = [(await lines.nth(0).boundingBox())!, (await lines.nth(1).boundingBox())!];
    // The dot and the lines form one block, centered in the banner.
    const block = (await lines.nth(0).locator("xpath=../..").boundingBox())!;
    const leftGap = block.x - box.x;
    const rightGap = box.x + box.width - (block.x + block.width);
    expect(Math.abs(leftGap - rightGap)).toBeLessThanOrEqual(2);
    expect(Math.abs(first.x - second.x)).toBeLessThanOrEqual(1);
    // The dot and × sit on the first line's center line.
    const firstMid = first.y + first.height / 2;
    const dot = (await banner.locator("span.rounded-full").first().boundingBox())!;
    expect(Math.abs(dot.y + dot.height / 2 - firstMid)).toBeLessThanOrEqual(2);
    const dismiss = (await banner.getByRole("button", { name: "Dismiss update notice" }).boundingBox())!;
    expect(Math.abs(dismiss.y + dismiss.height / 2 - firstMid)).toBeLessThanOrEqual(2);
    expect(box.x + box.width - (dismiss.x + dismiss.width)).toBeLessThanOrEqual(12);
  });
});
