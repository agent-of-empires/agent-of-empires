import { test, expect } from "./helpers/mockedTest";
import { Page } from "@playwright/test";

interface UpdateStatusFixture {
  update_check_mode: "auto" | "notify" | "off";
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string | null;
  error: string | null;
  // Persisted server-side (app_state). Acknowledgement is once-per-account, not
  // per browser, so the banner reads its dismissed version from here.
  dismissed_version?: string | null;
}

async function mockBase(page: Page) {
  await page.route("**/api/login/status", (r) => r.fulfill({ json: { required: false, authenticated: true } }));
  await page.route("**/api/sessions", (r) => r.fulfill({ json: { sessions: [], workspace_ordering: [] } }));
  for (const path of ["settings", "themes", "agents", "profiles", "groups", "devices", "docker/status", "about"]) {
    await page.route(`**/api/${path}`, (r) => r.fulfill({ json: path === "docker/status" ? {} : [] }));
  }
}

async function mock(page: Page, status: UpdateStatusFixture) {
  await mockBase(page);
  await page.route("**/api/system/update-status", (r) => r.fulfill({ json: status }));
}

test.describe("Update banner (#984, #1140)", () => {
  test("renders when update_available is true and mode is notify", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    const banner = page.getByRole("status", { name: /Update available/i });
    await expect(banner).toBeVisible();
    await expect(banner).toContainText("v0.5.0");
    await expect(banner).toContainText("v0.6.0");
    await expect(banner.getByRole("link", { name: "Release notes" })).toHaveAttribute(
      "href",
      "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
    );
  });

  test("hidden when update_check_mode is off (server suppresses)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "off",
      current_version: "0.5.0",
      latest_version: null,
      update_available: false,
      release_url: null,
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("hidden when update_check_mode is auto (background install)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "auto",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("hidden when latest matches current (no update)", async ({ page }) => {
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.6.0",
      latest_version: "0.6.0",
      update_available: false,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      error: null,
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("dismiss persists per-version across reload (server-side, once per account)", async ({ page }) => {
    // Server-backed dismissal: clicking Dismiss POSTs the version, and
    // subsequent update-status polls (this device or any other) report it
    // dismissed. Modelled with a mutable server-side flag.
    let dismissed: string | null = null;
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
    await page.getByRole("button", { name: /Dismiss update notice/i }).click();
    await expect(banner).toHaveCount(0);
    expect(dismissPosted).toBe(true);
    expect(dismissed).toBe("0.6.0");

    // Reload: the server now reports the version dismissed, so the banner
    // stays hidden without any per-browser state.
    await page.reload();
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("a version dismissed on another device is honored on first load", async ({ page }) => {
    // No local state seeded; the server reports 0.6.0 already dismissed.
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.6.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.6.0",
      error: null,
      dismissed_version: "0.6.0",
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toHaveCount(0);
  });

  test("dismissed version no longer suppresses a newer release", async ({ page }) => {
    // Server reports an older version dismissed; a newer release must show.
    await mock(page, {
      update_check_mode: "notify",
      current_version: "0.5.0",
      latest_version: "0.7.0",
      update_available: true,
      release_url: "https://github.com/agent-of-empires/agent-of-empires/releases/tag/v0.7.0",
      error: null,
      dismissed_version: "0.6.0",
    });
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.goto("/");
    await expect(page.locator("header")).toBeVisible();
    await expect(page.getByRole("status", { name: /Update available/i })).toBeVisible();
  });
});
