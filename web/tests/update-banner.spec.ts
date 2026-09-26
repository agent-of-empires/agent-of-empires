import { test, expect, waitForResponseBody, observeFor } from "./helpers/mockedTest";
import { mockStaticApis } from "./helpers/apiMocks";
import { Page } from "@playwright/test";

async function mockBase(page: Page) {
  await mockStaticApis(page);
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
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
});
